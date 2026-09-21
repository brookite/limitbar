//! Prints what the providers see, without starting a window.
//!
//! Useful for checking the widget's numbers against `claude /usage` and
//! `codex /status`, and for diagnosing a section that shows an error. It prints
//! percentages and reset times only — never any part of a credential.
//!
//!     cargo run -p limitbar-core --example probe

#[tokio::main]
async fn main() {
    let mut monitor = limitbar_core::Monitor::new(limitbar_core::default_providers());
    let snapshot = monitor.refresh().await;

    for agent in &snapshot.agents {
        let note = agent.note.as_deref().unwrap_or("-");
        println!("{} [{}]  {:?}", agent.display_name, note, agent.status);
        for gauge in &agent.gauges {
            let resets = match gauge.resets_at {
                Some(at) => format!("resets in {}s", at - snapshot.fetched_at),
                None => "no reset reported".to_string(),
            };
            println!(
                "  {:<14} {:>5.1}%  {:?}  ({resets})",
                gauge.label, gauge.used_percent, gauge.severity
            );
        }
        println!();
    }
}
