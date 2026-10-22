//! `djvu optimize`, plus the atomic output writers other commands share.

use super::*;

pub(super) fn cmd_optimize(
    input: &Path,
    output: &Path,
    preset: OptimizePresetArg,
    target_size: Option<u64>,
    max_ssim_loss: Option<f32>,
    lossy_text: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let input_bytes = std::fs::read(input)?;
    if equivalent_paths(input, output)? {
        return Err(
            "optimizer refuses to replace the input file; choose a different --output".into(),
        );
    }

    let preset = match preset {
        OptimizePresetArg::LosslessCleanup => {
            djvu_rs::optimizer::OptimizationPreset::LosslessCleanup
        }
        OptimizePresetArg::Archival => djvu_rs::optimizer::OptimizationPreset::Archival,
    };
    let mut request = djvu_rs::optimizer::OptimizationRequest::new(preset);
    if let Some(target) = target_size {
        request = request.with_target_size(target);
    }
    if let Some(loss) = max_ssim_loss {
        request = request.with_max_ssim_loss(loss);
    }
    if lossy_text {
        request = request.with_lossy_text(true);
    }

    // A progress line on an interactive stderr only: the JSON on stdout is
    // the machine-readable contract and a pipe must not see the line either.
    use std::io::IsTerminal;
    let show_progress = std::io::stderr().is_terminal();
    let mut optimizer = djvu_rs::optimizer::Optimizer::new(request);
    if show_progress {
        optimizer = optimizer.with_progress(|event| {
            eprint!(
                "\r\x1b[K{} {}/{} {} {} B",
                event.phase.as_str(),
                event.component_index + 1,
                event.component_count,
                String::from_utf8_lossy(&event.component_id),
                event.bytes_so_far
            );
        });
    }
    let end_progress = || {
        if show_progress {
            eprint!("\r\x1b[K");
        }
    };
    if dry_run {
        let plan = optimizer.plan(&input_bytes);
        end_progress();
        println!("{}", plan?.to_json());
        return Ok(());
    }

    let result = optimizer.optimize(&input_bytes);
    end_progress();
    let result = result?;
    write_atomic(output, &result.bytes)?;
    println!("{}", result.report.to_json());
    Ok(())
}

pub(super) fn equivalent_paths(
    input: &Path,
    output: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let input = std::fs::canonicalize(input)?;
    let output = if output.exists() {
        std::fs::canonicalize(output)?
    } else {
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        std::fs::canonicalize(parent)?.join(output.file_name().ok_or("--output must name a file")?)
    };
    Ok(input == output)
}

pub(super) fn write_atomic(output: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    write_atomic_with(output, |mut file| {
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })
}

/// Write an output through a sibling temporary path, committing it only after
/// `write` succeeds. The temporary path is always removed when `write` or the
/// final rename fails, so an existing destination is left untouched on error.
pub(super) fn write_atomic_with<F>(
    output: &Path,
    write: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(std::fs::File) -> Result<(), Box<dyn std::error::Error>>,
{
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .ok_or("--output must name a file")?
        .to_string_lossy();
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    if let Err(error) = write(file) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, output) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}
