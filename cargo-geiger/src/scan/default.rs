mod table;

use crate::args::FeaturesArgs;
use crate::format::print_config::OutputFormat;
use crate::graph::Graph;
use crate::mapping::CargoMetadataParameters;
use crate::scan::rs_file::resolve_rs_file_deps;

use super::find::find_unsafe;
use super::{
    list_files_used_but_not_scanned, package_metrics, unsafe_stats,
    ScanDetails, ScanMode, ScanParameters,
};

use table::scan_to_table;

use cargo::core::compiler::CompileMode;
use cargo::core::Workspace;
use cargo::ops::CompileOptions;
use cargo::{CliError, CliResult, Config};
use cargo_geiger_serde::{ReportEntry, SafetyReport};
use cargo_metadata::PackageId;

pub fn scan_unsafe(
    cargo_metadata_parameters: &CargoMetadataParameters,
    graph: &Graph,
    root_package_id: PackageId,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> CliResult {
    match scan_parameters.args.output_format {
        Some(output_format) => scan_to_report(
            cargo_metadata_parameters,
            graph,
            output_format,
            root_package_id,
            scan_parameters,
            workspace,
        ),
        None => scan_to_table(
            cargo_metadata_parameters,
            graph,
            root_package_id,
            scan_parameters,
            workspace,
        ),
    }
}

fn suffixed_file_name(
    path: impl AsRef<std::path::Path>,
    suffix: impl AsRef<std::ffi::OsStr>,
) -> std::ffi::OsString {
    let path = path.as_ref();
    let suffix = suffix.as_ref();

    let stem = path.file_stem().unwrap();
    let ext = path.extension().unwrap_or_default();

    let mut new_file_name = std::ffi::OsString::new();
    new_file_name.push(stem);
    new_file_name.push(suffix);
    new_file_name.push(ext);

    new_file_name
}

fn log_unsafe_functions(
    log_path: impl AsRef<std::path::Path>,
    report: &SafetyReport,
) -> anyhow::Result<()> {
    use std::io::Write;

    let log_path = log_path.as_ref();

    let declared_log =
        std::fs::File::create(suffixed_file_name(log_path, ".declared"))?;
    let mut declared_writer = std::io::BufWriter::new(declared_log);

    let contains_log =
        std::fs::File::create(suffixed_file_name(log_path, ".contains"))?;
    let mut contains_writer = std::io::BufWriter::new(contains_log);

    for (package_id, report_entry) in report.packages.iter() {
        for name in &report_entry.unsafety.declared_unsafe_functions {
            write!(&mut declared_writer, "{} {}\n", package_id.name, name)?;
        }
        for name in &report_entry.unsafety.contains_unsafe_functions {
            write!(&mut contains_writer, "{} {}\n", package_id.name, name)?;
        }
    }

    Ok(())
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
fn build_compile_options<'a>(
    args: &'a FeaturesArgs,
    config: &'a Config,
) -> CompileOptions {
    let mut compile_options =
        CompileOptions::new(&config, CompileMode::Check { test: false })
            .unwrap();
    compile_options.features = args.features.clone();
    compile_options.all_features = args.all_features;
    compile_options.no_default_features = args.no_default_features;

    // TODO: Investigate if this is relevant to cargo-geiger.
    //let mut bins = Vec::new();
    //let mut examples = Vec::new();
    // opt.release = args.release;
    // opt.target = args.target.clone();
    // if let Some(ref name) = args.bin {
    //     bins.push(name.clone());
    // } else if let Some(ref name) = args.example {
    //     examples.push(name.clone());
    // }
    // if args.bin.is_some() || args.example.is_some() {
    //     opt.filter = ops::CompileFilter::new(
    //         false,
    //         bins.clone(), false,
    //         Vec::new(), false,
    //         examples.clone(), false,
    //         Vec::new(), false,
    //         false,
    //     );
    // }

    compile_options
}

fn scan(
    cargo_metadata_parameters: &CargoMetadataParameters,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> Result<ScanDetails, CliError> {
    let compile_options = build_compile_options(
        &scan_parameters.args.features_args,
        scan_parameters.config,
    );
    let rs_files_used =
        resolve_rs_file_deps(&compile_options, workspace).unwrap();
    let geiger_context = find_unsafe(
        cargo_metadata_parameters,
        scan_parameters.config,
        ScanMode::Full,
        scan_parameters.print_config,
    )?;
    Ok(ScanDetails {
        rs_files_used,
        geiger_context,
    })
}

fn scan_to_report(
    cargo_metadata_parameters: &CargoMetadataParameters,
    graph: &Graph,
    output_format: OutputFormat,
    root_package_id: PackageId,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> CliResult {
    let ScanDetails {
        rs_files_used,
        geiger_context,
    } = scan(cargo_metadata_parameters, scan_parameters, workspace)?;
    let mut report = SafetyReport::default();
    for (package, package_metrics_option) in package_metrics(
        cargo_metadata_parameters,
        &geiger_context,
        graph,
        root_package_id,
    ) {
        let package_metrics = match package_metrics_option {
            Some(m) => m,
            None => {
                report.packages_without_metrics.insert(package.id);
                continue;
            }
        };
        let unsafe_info = unsafe_stats(&package_metrics, &rs_files_used);
        let entry = ReportEntry {
            package,
            unsafety: unsafe_info,
        };
        report.packages.insert(entry.package.id.clone(), entry);
    }
    report.used_but_not_scanned_files =
        list_files_used_but_not_scanned(&geiger_context, &rs_files_used)
            .into_iter()
            .collect();
    let s = match output_format {
        OutputFormat::Json => serde_json::to_string(&report).unwrap(),
    };
    println!("{}", s);
    if let Some(log_path) = &scan_parameters.args.unsafe_fn_log {
        log_unsafe_functions(log_path, &report)?;
    }
    Ok(())
}

#[cfg(test)]
mod default_tests {
    use super::*;
    use rstest::*;

    #[rstest(
        input_features,
        expected_compile_features,
        case(
            vec![
                String::from("unit"),
                String::from("test"),
                String::from("features")
            ],
            vec!["unit", "test", "features"],
        ),
        case(
            vec![String::from("")],
            vec![""],
        )
    )]
    fn build_compile_options_test(
        input_features: Vec<String>,
        expected_compile_features: Vec<&str>,
    ) {
        let args = FeaturesArgs {
            all_features: rand::random(),
            features: input_features,
            no_default_features: rand::random(),
        };

        let config = Config::default().unwrap();
        let compile_options = build_compile_options(&args, &config);

        assert_eq!(compile_options.all_features, args.all_features);
        assert_eq!(compile_options.features, expected_compile_features);
        assert_eq!(
            compile_options.no_default_features,
            args.no_default_features
        );
    }
}
