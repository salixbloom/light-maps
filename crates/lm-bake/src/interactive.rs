/// Interactive pre-bake picker (`--interactive`).
///
/// Shows a sampled estimate of the bake — projected feature count, output size,
/// peak RAM — then a checklist of property fields (heaviest first, each labelled
/// with its projected dataset cost) so the user can drop fields they don't need
/// before committing to a multi-minute bake. Returns the kept-field set, or
/// `None` if the user aborted.
use std::collections::HashSet;

use inquire::{Confirm, MultiSelect};

use crate::estimate::{human_bytes, Estimate};

/// Outcome of the interactive picker.
pub enum Choice {
    /// Proceed, keeping exactly these fields (empty set = drop all properties).
    Proceed(HashSet<String>),
    /// User aborted the bake.
    Abort,
}

/// Run the picker against a sampled estimate. `is_tty` guards are the caller's
/// responsibility — this assumes an interactive terminal.
pub fn pick_fields(est: &Estimate) -> Choice {
    print_summary(est);

    if est.fields.is_empty() {
        // Nothing to choose; just confirm.
        return match confirm_proceed() {
            true => Choice::Proceed(HashSet::new()),
            false => Choice::Abort,
        };
    }

    // Build checklist options: "FIELD_NAME   (kind, ~265 MB)".
    let labels: Vec<String> = est
        .fields
        .iter()
        .map(|f| {
            format!(
                "{}   ({}, ~{})",
                f.name,
                f.kind,
                human_bytes(f.total_bytes(est.feature_count))
            )
        })
        .collect();

    // All fields pre-selected (default: keep everything).
    let all_indices: Vec<usize> = (0..labels.len()).collect();

    let selection = MultiSelect::new("Fields to keep (space toggles, enter confirms):", labels.clone())
        .with_default(&all_indices)
        .with_page_size(15)
        .with_help_message("↑↓ move · space toggle · → all · ← none · enter confirm · esc abort")
        .raw_prompt();

    let chosen = match selection {
        Ok(items) => items,
        Err(_) => return Choice::Abort, // esc / interrupt
    };

    let keep: HashSet<String> = chosen
        .iter()
        .map(|item| est.fields[item.index].name.clone())
        .collect();

    // Show the revised projection for the trimmed set before the final confirm.
    print_revised(est, &keep);

    match confirm_proceed() {
        true => Choice::Proceed(keep),
        false => Choice::Abort,
    }
}

fn print_summary(est: &Estimate) {
    let count_note = if est.exact { "" } else { " (estimated)" };
    eprintln!();
    eprintln!("  ── bake estimate ─────────────────────────────────────────");
    eprintln!("  input            {}", human_bytes(est.file_bytes));
    eprintln!("  features         {}{}", est.feature_count, count_note);
    eprintln!("  property fields  {}", est.fields.len());
    eprintln!(
        "  output (approx)  {}",
        human_bytes(est.output_bytes(None))
    );
    eprintln!(
        "  peak RAM (unreclaimable)  ~{}",
        human_bytes(est.peak_unreclaimable_bytes(None))
    );
    eprintln!(
        "    + reclaimable cache     ~{}  (mmap store; dropped under pressure)",
        human_bytes(est.reclaimable_cache_bytes(None))
    );
    eprintln!("  ──────────────────────────────────────────────────────────");
    eprintln!("  (sizes are sampled projections, not guarantees;");
    eprintln!("   RSS ≈ unreclaimable + cache, but only the first risks OOM)");
    eprintln!();
}

fn print_revised(est: &Estimate, keep: &HashSet<String>) {
    let kept = Some(keep);
    let dropped = est.fields.len() - keep.len();
    eprintln!();
    eprintln!(
        "  keeping {} of {} fields ({} dropped)",
        keep.len(),
        est.fields.len(),
        dropped
    );
    eprintln!(
        "  revised output   ~{}   peak RAM (unreclaimable) ~{}",
        human_bytes(est.output_bytes(kept)),
        human_bytes(est.peak_unreclaimable_bytes(kept))
    );
    eprintln!();
}

fn confirm_proceed() -> bool {
    Confirm::new("Proceed with bake?")
        .with_default(true)
        .prompt()
        .unwrap_or(false)
}
