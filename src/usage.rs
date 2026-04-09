use crate::jsonl::JsonlMessage;
use crate::models::AggregatedTokens;
use crate::pricing::compute_cost;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ============================================================================
// JSONL Usage Parsing (replaces token log parsing for cost)
// ============================================================================

/// A single aggregated usage record per session+model
#[derive(Debug, Clone, Default)]
struct UsageRecord {
    pub session_id: String,
    pub date: String,
    pub provider: String,
    pub project: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
}

impl UsageRecord {
    pub fn cost(&self) -> f64 {
        compute_cost(
            &self.model,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        )
    }
}

/// Find all JSONL files across all provider directories (matching token-usage skill)
pub fn find_jsonl_files() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut files = Vec::new();

    // Collect all provider directories: ~/.claude, ~/.claude-*, etc.
    let mut provider_dirs: Vec<PathBuf> = Vec::new();

    // Standard provider
    let standard = PathBuf::from(&home).join(".claude");
    if standard.exists() && standard.join("projects").exists() {
        provider_dirs.push(standard);
    }

    // Additional providers: ~/.claude-*/*/projects
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".claude-") && entry.path().is_dir() {
                let projects = entry.path().join("projects");
                if projects.exists() {
                    provider_dirs.push(entry.path());
                }
            }
        }
    }

    for provider_dir in provider_dirs {
        let projects_dir = provider_dir.join("projects");
        if let Ok(entries) = std::fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let project_path = entry.path();
                if project_path.is_dir() {
                    if let Ok(jsonl_entries) = std::fs::read_dir(&project_path) {
                        for jsonl_entry in jsonl_entries.flatten() {
                            let path = jsonl_entry.path();
                            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                                files.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    files
}

/// Process a single JSONL file and extract usage records
fn process_jsonl_file(path: &Path) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Derive provider, project, session_id from path
    let home = std::env::var("HOME").unwrap_or_default();
    let rel_path_raw = path.to_string_lossy().replace(&home, "");
    let rel_path = rel_path_raw.trim_start_matches('/');
    let parts: Vec<&str> = rel_path.split('/').collect();

    let provider = if parts
        .first()
        .map(|s| s.starts_with(".claude-"))
        .unwrap_or(false)
    {
        parts.first().unwrap_or(&".claude").to_string()
    } else {
        ".claude".to_string()
    };

    // project is between "projects" and filename
    let project = if let Some(idx) = parts.iter().position(|s| *s == "projects") {
        parts.get(idx + 1).unwrap_or(&"").to_string()
    } else {
        String::new()
    };

    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut records: Vec<UsageRecord> = Vec::new();
    let mut seen_uuids: HashSet<String> = std::collections::HashSet::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Parse as JSON
        if let Ok(msg) = serde_json::from_str::<JsonlMessage>(line) {
            if msg.msg_type == "assistant" {
                // Deduplicate by uuid (skip if already seen)
                if let Some(ref uuid) = msg.uuid {
                    if seen_uuids.contains(uuid) {
                        continue;
                    }
                    seen_uuids.insert(uuid.clone());
                }

                if let Some(ref message) = msg.message {
                    if let Some(ref usage) = message.usage {
                        let model = message.model.as_deref().unwrap_or("unknown").to_string();

                        let input = usage.input_tokens.unwrap_or(0);
                        let output = usage.output_tokens.unwrap_or(0);
                        let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
                        let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);

                        if input > 0 || output > 0 || cache_read > 0 || cache_write > 0 {
                            records.push(UsageRecord {
                                session_id: session_id.clone(),
                                date: msg.timestamp.clone().unwrap_or_default(),
                                provider: provider.clone(),
                                project: project.clone(),
                                model,
                                input_tokens: input,
                                output_tokens: output,
                                cache_read_tokens: cache_read,
                                cache_write_tokens: cache_write,
                                total_tokens: input + output + cache_read + cache_write,
                                message_count: 1,
                            });
                        }
                    }
                }
            }
        }
    }

    records
}

/// Aggregate usage records by session+model+date (matching token-usage skill)
fn aggregate_usage(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    use std::collections::HashMap;

    let mut aggregated: HashMap<String, UsageRecord> = HashMap::new();

    for record in records {
        let key = format!(
            "{}|{}|{}|{}|{}",
            record.session_id, record.date, record.provider, record.project, record.model
        );

        let entry = aggregated.entry(key).or_insert_with(|| UsageRecord {
            session_id: record.session_id.clone(),
            date: record.date.clone(),
            provider: record.provider.clone(),
            project: record.project.clone(),
            model: record.model.clone(),
            ..Default::default()
        });

        entry.input_tokens += record.input_tokens;
        entry.output_tokens += record.output_tokens;
        entry.cache_read_tokens += record.cache_read_tokens;
        entry.cache_write_tokens += record.cache_write_tokens;
        entry.total_tokens += record.total_tokens;
        entry.message_count += record.message_count;
    }

    aggregated.into_values().collect()
}

/// Scan all JSONL files and compute aggregated costs (matching token-usage skill cost mode)
pub fn scan_all_usage() -> AggregatedTokens {
    let files = find_jsonl_files();

    let mut all_records: Vec<UsageRecord> = Vec::new();
    for path in files {
        all_records.extend(process_jsonl_file(&path));
    }

    let aggregated = aggregate_usage(all_records);

    let today = Utc::now().date_naive();

    let mut result = AggregatedTokens::default();
    let mut hourly_tokens: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

    for record in aggregated {
        let cost = record.cost();

        // All-time totals
        result.total_tokens += record.total_tokens;
        result.total_cost += cost;
        result.total_input += record.input_tokens;
        result.total_output += record.output_tokens;
        result.total_cache_read += record.cache_read_tokens;
        result.total_cache_write += record.cache_write_tokens;

        // Parse timestamp once for daily and hourly grouping
        if let Ok(ts) = DateTime::parse_from_rfc3339(&record.date) {
            let utc_date = ts.with_timezone(&Utc).date_naive();
            if utc_date == today {
                result.today_tokens += record.total_tokens;
                result.today_cost += cost;
                result.today_input += record.input_tokens;
                result.today_output += record.output_tokens;
                result.today_cache_read += record.cache_read_tokens;
                result.today_cache_write += record.cache_write_tokens;
            }
            // Hourly rate (for charts if needed)
            let hour_key = ts.timestamp() / 3600;
            *hourly_tokens.entry(hour_key).or_insert(0) += record.total_tokens;
        }
    }

    // Build 24-hour rate array
    let now_hour = Utc::now().timestamp() / 3600;
    for i in 0..24 {
        let hour = now_hour - i;
        result
            .hourly_rates
            .push(hourly_tokens.get(&hour).copied().unwrap_or(0));
    }
    result.hourly_rates.reverse();

    result
}
