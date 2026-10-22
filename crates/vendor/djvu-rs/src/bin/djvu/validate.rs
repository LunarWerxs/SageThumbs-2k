//! `djvu validate` and `djvu diff`.

use super::*;

pub(super) fn cmd_validate(
    path: &Path,
    strict: bool,
    json: bool,
    decode_pages: bool,
    limits_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path).map_err(|error| ValidateExit {
        code: 2,
        silent: false,
        message: format!("cannot read {}: {error}", path.display()),
    })?;
    let limits = match limits_path {
        Some(limits_path) => Some(load_limits(limits_path)?),
        None => None,
    };
    let options = ValidateOptions {
        strict,
        decode_pages,
        limits,
    };
    let report = djvu_rs::validate::validate(&data, &options);
    let summary = report.summary();

    if json {
        println!("{}", serde_json::to_string(&validate_json(path, &report))?);
    } else {
        print_validate_human(&report);
    }

    if !report.is_valid() || (strict && summary.warnings > 0) {
        return Err(Box::new(ValidateExit {
            code: 1,
            silent: true,
            message: String::new(),
        }));
    }
    Ok(())
}

/// Load configured resource limits from a JSON file. Unknown keys and
/// non-integer values are rejected so a mistyped limit fails loudly rather than
/// silently disabling a guard. Every failure maps to the read/parse exit code 2.
pub(super) fn load_limits(path: &Path) -> Result<ResourceLimits, Box<dyn std::error::Error>> {
    let fail = |message: String| ValidateExit {
        code: 2,
        silent: false,
        message,
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| fail(format!("cannot read limits {}: {error}", path.display())))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| fail(format!("cannot parse limits {}: {error}", path.display())))?;
    let object = value
        .as_object()
        .ok_or_else(|| fail(format!("limits {} must be a JSON object", path.display())))?;

    const KNOWN: [&str; 7] = [
        "max_file_bytes",
        "max_pages",
        "max_components",
        "max_page_pixels",
        "max_total_pixels",
        "max_decoded_bytes",
        "max_render_pixels",
    ];
    for key in object.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(Box::new(fail(format!(
                "limits {}: unknown key '{key}'",
                path.display()
            ))));
        }
    }

    let read = |key: &str| -> Result<Option<u64>, Box<dyn std::error::Error>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) if number.is_u64() => Ok(number.as_u64()),
            Some(_) => Err(Box::new(fail(format!(
                "limits {}: '{key}' must be a non-negative integer",
                path.display()
            ))) as Box<dyn std::error::Error>),
        }
    };

    Ok(ResourceLimits {
        max_file_bytes: read("max_file_bytes")?,
        max_pages: read("max_pages")?,
        max_components: read("max_components")?,
        max_page_pixels: read("max_page_pixels")?,
        max_total_pixels: read("max_total_pixels")?,
        max_decoded_bytes: read("max_decoded_bytes")?,
        max_render_pixels: read("max_render_pixels")?,
    })
}

pub(super) fn cmd_diff(
    a: &Path,
    b: &Path,
    json: bool,
    planes: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let read = |path: &Path| {
        std::fs::read(path).map_err(|error| ValidateExit {
            code: 2,
            silent: false,
            message: format!("cannot read {}: {error}", path.display()),
        })
    };
    let bytes_a = read(a)?;
    let bytes_b = read(b)?;
    let filter = (!planes.is_empty()).then_some(planes);
    let diff =
        djvu_rs::semantic_diff::semantic_diff(&bytes_a, &bytes_b, filter).map_err(|error| {
            ValidateExit {
                code: 2,
                silent: false,
                message: format!("cannot parse inputs: {error}"),
            }
        })?;

    if json {
        let planes_json: Vec<serde_json::Value> = diff
            .planes
            .iter()
            .map(|plane| {
                serde_json::json!({
                    "plane": plane.plane,
                    "status": match plane.status {
                        djvu_rs::semantic_diff::PlaneStatus::Match => "match",
                        djvu_rs::semantic_diff::PlaneStatus::Diverge => "diverge",
                    },
                    "details": plane.details,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "a": a.display().to_string(),
                "b": b.display().to_string(),
                "identical": diff.is_identical(),
                "planes": planes_json,
            })
        );
    } else {
        for plane in &diff.planes {
            match plane.status {
                djvu_rs::semantic_diff::PlaneStatus::Match => {
                    println!("{}: match", plane.plane);
                }
                djvu_rs::semantic_diff::PlaneStatus::Diverge => {
                    println!("{}: diverge", plane.plane);
                    for detail in &plane.details {
                        println!("  {detail}");
                    }
                }
            }
        }
    }

    if !diff.is_identical() {
        return Err(Box::new(ValidateExit {
            code: 1,
            silent: true,
            message: String::new(),
        }));
    }
    Ok(())
}

pub(super) fn print_validate_human(report: &ValidationReport) {
    for layer in [
        ValidationLayer::Structural,
        ValidationLayer::Dependency,
        ValidationLayer::Codec,
        ValidationLayer::Semantic,
        ValidationLayer::Resource,
    ] {
        let findings = report
            .findings
            .iter()
            .filter(|finding| finding.layer == layer)
            .collect::<Vec<_>>();
        if findings.is_empty() {
            continue;
        }
        println!("{}:", layer.as_str());
        for finding in findings {
            let location = match (&finding.component, &finding.chunk, finding.offset) {
                (Some(component), Some(chunk), Some(offset)) => {
                    format!(" [{component} {chunk} @ {offset}]")
                }
                (Some(component), Some(chunk), None) => format!(" [{component} {chunk}]"),
                (Some(component), None, Some(offset)) => format!(" [{component} @ {offset}]"),
                (None, Some(chunk), Some(offset)) => format!(" [{chunk} @ {offset}]"),
                (Some(component), None, None) => format!(" [{component}]"),
                (None, Some(chunk), None) => format!(" [{chunk}]"),
                (None, None, Some(offset)) => format!(" [@ {offset}]"),
                (None, None, None) => String::new(),
            };
            println!(
                "  {} {}{}: {}",
                finding.severity.as_str().to_uppercase(),
                finding.code,
                location,
                finding.message
            );
        }
    }
    let resources = &report.resources;
    println!(
        "resources: {} pages, {} components, {} bytes, {} peak page pixels, {} est. peak decoded bytes",
        resources.pages,
        resources.components,
        resources.file_bytes,
        resources.max_page_pixels,
        resources.peak_decoded_bytes,
    );
    let summary = report.summary();
    println!(
        "{} errors, {} warnings, {} tolerated, {} recovery",
        summary.errors, summary.warnings, summary.tolerated, summary.recovery
    );
}

pub(super) fn validate_json(path: &Path, report: &ValidationReport) -> Value {
    let summary = report.summary();
    let resources = &report.resources;
    json!({
        "file": path.display().to_string(),
        "valid": report.is_valid(),
        "summary": {
            "errors": summary.errors,
            "warnings": summary.warnings,
            "tolerated": summary.tolerated,
            "recovery": summary.recovery,
        },
        "resources": {
            "file_bytes": resources.file_bytes,
            "pages": resources.pages,
            "components": resources.components,
            "max_page_pixels": resources.max_page_pixels,
            "total_pixels": resources.total_pixels,
            "peak_decoded_bytes": resources.peak_decoded_bytes,
        },
        "findings": report.findings.iter().map(|finding| json!({
            "severity": finding.severity.as_str(),
            "layer": finding.layer.as_str(),
            "code": finding.code,
            "component": &finding.component,
            "chunk": &finding.chunk,
            "offset": finding.offset,
            "message": &finding.message,
        })).collect::<Vec<_>>(),
    })
}
