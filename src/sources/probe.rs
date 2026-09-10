//! SSH host probing for remote source setup.
//!
//! This module provides functionality to probe SSH hosts and gather comprehensive
//! information needed for remote source configuration decisions:
//! - Whether cass is installed (and what version)
//! - Index status (session count)
//! - Detected agent session data directories
//! - System information (OS, architecture)
//! - Resource availability (disk space, memory)
//!
//! # Design
//!
//! Probing uses a single SSH session per host to minimize latency. A bash probe
//! script is piped to `bash -s` on the remote, gathering all information in one
//! round-trip.
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::probe::{probe_host, probe_hosts_parallel};
//! use coding_agent_search::sources::config::DiscoveredHost;
//!
//! // Single host probe (returns HostProbeResult directly, not Result)
//! let host = DiscoveredHost { name: "laptop".into(), .. };
//! let result = probe_host(&host, 10);
//! if result.reachable {
//!     println!("Connected in {}ms", result.connection_time_ms);
//! }
//!
//! // Parallel probing with progress (synchronous, uses rayon internally)
//! let results = probe_hosts_parallel(&hosts, 10, |done, total, name| {
//!     println!("Probing {}/{}: {}", done, total, name);
//! });
//! ```

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    config::DiscoveredHost, configure_child_process_group, file_backed_child_stdin,
    host_key_verification_error, is_host_key_verification_failure, strict_ssh_cli_tokens,
    wait_for_child_output_with_timeout,
};

/// Default connection timeout in seconds.
pub const DEFAULT_PROBE_TIMEOUT: u64 = 10;

/// Result of probing an SSH host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProbeResult {
    /// SSH config host alias.
    pub host_name: String,
    /// Whether the host was reachable via SSH.
    pub reachable: bool,
    /// Connection time in milliseconds.
    pub connection_time_ms: u64,
    /// Status of cass installation on the remote.
    pub cass_status: CassStatus,
    /// Detected agent session directories.
    pub detected_agents: Vec<DetectedAgent>,
    /// System information.
    pub system_info: Option<SystemInfo>,
    /// Resource information (disk/memory).
    pub resources: Option<ResourceInfo>,
    /// Error message if probe failed.
    pub error: Option<String>,
}

impl HostProbeResult {
    /// Create a result for an unreachable host.
    pub fn unreachable(host_name: &str, error: impl Into<String>) -> Self {
        Self {
            host_name: host_name.to_string(),
            reachable: false,
            connection_time_ms: 0,
            cass_status: CassStatus::Unknown,
            detected_agents: Vec::new(),
            system_info: None,
            resources: None,
            error: Some(error.into()),
        }
    }

    /// Check if cass is installed on this host.
    pub fn has_cass(&self) -> bool {
        self.cass_status.is_installed()
    }

    /// Check if this host has any agent session data.
    pub fn has_agent_data(&self) -> bool {
        !self.detected_agents.is_empty()
    }

    /// Get total estimated sessions across all detected agents.
    pub fn total_sessions(&self) -> u64 {
        self.detected_agents
            .iter()
            .filter_map(|a| a.estimated_sessions)
            .sum()
    }
}

/// Status of cass installation on a remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CassStatus {
    /// cass is installed and has an indexed database.
    Indexed {
        version: String,
        session_count: u64,
        last_indexed: Option<String>,
    },
    /// cass is installed but no index exists or is empty.
    InstalledNotIndexed { version: String },
    /// cass is not found on PATH.
    NotFound,
    /// Couldn't determine cass status.
    Unknown,
}

impl CassStatus {
    /// Check if cass is installed (any version).
    pub fn is_installed(&self) -> bool {
        matches!(
            self,
            CassStatus::Indexed { .. } | CassStatus::InstalledNotIndexed { .. }
        )
    }

    /// Get the installed version if available.
    pub fn version(&self) -> Option<&str> {
        match self {
            CassStatus::Indexed { version, .. } | CassStatus::InstalledNotIndexed { version } => {
                Some(version)
            }
            _ => None,
        }
    }
}

/// Detected agent session data on a remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedAgent {
    /// Type of agent (claude_code, codex, cursor, etc.).
    pub agent_type: String,
    /// Path to the agent's session directory.
    pub path: String,
    /// Estimated number of sessions (from file count).
    pub estimated_sessions: Option<u64>,
    /// Estimated size in megabytes.
    pub estimated_size_mb: Option<u64>,
}

/// System information gathered from remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Operating system (linux, darwin).
    pub os: String,
    /// CPU architecture (x86_64, aarch64).
    pub arch: String,
    /// Linux distro name if available.
    pub distro: Option<String>,
    /// Whether cargo is available.
    pub has_cargo: bool,
    /// Whether cargo-binstall is available.
    pub has_cargo_binstall: bool,
    /// Whether curl is available.
    pub has_curl: bool,
    /// Whether wget is available.
    pub has_wget: bool,
    /// Remote home directory.
    pub remote_home: String,
    /// Unique machine identifier (for deduplication of SSH aliases).
    /// On Linux: /etc/machine-id, on macOS: IOPlatformUUID.
    #[serde(default)]
    pub machine_id: Option<String>,
}

/// Resource information for installation feasibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    /// Available disk space in MB (in home directory).
    pub disk_available_mb: u64,
    /// Total memory in MB.
    pub memory_total_mb: u64,
    /// Available memory in MB.
    pub memory_available_mb: u64,
    /// Heuristic: enough resources to compile Rust.
    pub can_compile: bool,
}

impl ResourceInfo {
    /// Minimum disk space (MB) recommended for cass installation.
    pub const MIN_DISK_MB: u64 = 1024; // 1 GB

    /// Minimum memory (MB) recommended for compilation.
    pub const MIN_MEMORY_MB: u64 = 2048; // 2 GB
}

fn shell_single_quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn collect_probe_dirs(probe_paths: Vec<(&'static str, Vec<String>)>) -> Vec<String> {
    let mut dir_list = Vec::new();
    for (_slug, paths) in probe_paths {
        for path in paths {
            dir_list.push(path);
        }
    }
    dir_list.sort();
    dir_list.dedup();
    dir_list
}

/// Return true when `parent` is a path ancestor of `child`.
///
/// The probe list deliberately contains both source roots and their parent
/// configuration directories (for example `~/.claude/projects` and
/// `~/.claude`). Comparing complete path components keeps a parent such as
/// `~/.claude` from being confused with an unrelated path like
/// `~/.claude-work`.
fn is_path_ancestor(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Select only the leaf paths for recursive statistics probes.
///
/// Parent paths remain in `PROBE_DIRS` so setup can still discover an
/// installed agent whose session directory is not present yet. They are
/// presence probes only; recursively walking them would count caches and
/// transcripts repeatedly and can make a read-only SSH probe unbounded.
fn recursive_probe_dirs(dir_list: &[String]) -> Vec<String> {
    dir_list
        .iter()
        .filter(|candidate| {
            !dir_list
                .iter()
                .any(|child| is_path_ancestor(candidate, child))
        })
        .cloned()
        .collect()
}

fn probe_dir_array_entries(dir_list: &[String]) -> String {
    dir_list
        .iter()
        .map(|path| format!("    {}", shell_single_quote_arg(path)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the bash probe script that gathers all information in one SSH call.
///
/// Agent detection paths are sourced dynamically from `franken_agent_detection`
/// so that new connectors are automatically included in SSH probes.
///
/// Output format is key=value pairs, with special markers for sections.
fn build_probe_script() -> String {
    let dir_list = collect_probe_dirs(franken_agent_detection::default_probe_paths_tilde());
    let recursive_dirs = recursive_probe_dirs(&dir_list);
    build_probe_script_for_dir_modes(&dir_list, &recursive_dirs)
}

fn build_probe_script_for_dirs(dir_list: &[String]) -> String {
    // This helper is used by focused tests with an explicit path list. Treat
    // those paths as source roots so the tests exercise the same bounded
    // recursive branch as a dynamically discovered leaf path.
    build_probe_script_for_dir_modes(dir_list, dir_list)
}

fn build_probe_script_for_dir_modes(dir_list: &[String], recursive_dirs: &[String]) -> String {
    let dirs_str = probe_dir_array_entries(dir_list);
    let recursive_dirs_str = probe_dir_array_entries(recursive_dirs);

    format!(
        r#"#!/bin/bash
echo "===PROBE_START==="

# System info
echo "OS=$(uname -s | tr '[:upper:]' '[:lower:]')"
echo "ARCH=$(uname -m)"
echo "HOME=$HOME"

# Distro detection (Linux only)
if [ -f /etc/os-release ]; then
    . /etc/os-release
    echo "DISTRO=$PRETTY_NAME"
fi

# Machine ID for deduplication of SSH aliases pointing to same host
# Linux: /etc/machine-id, macOS: IOPlatformUUID
if [ -f /etc/machine-id ]; then
    MACHINE_ID=$(cat /etc/machine-id 2>/dev/null | tr -d '\n')
    echo "MACHINE_ID=$MACHINE_ID"
elif command -v ioreg &> /dev/null; then
    MACHINE_ID=$(ioreg -rd1 -c IOPlatformExpertDevice 2>/dev/null | awk -F'"' '/IOPlatformUUID/{{print $4}}')
    echo "MACHINE_ID=$MACHINE_ID"
fi

# Cass status - check PATH and common install locations
# Non-interactive SSH doesn't source .bashrc, so user bin dirs may not be in PATH
CASS_BIN=""
if command -v cass &> /dev/null; then
    CASS_BIN="cass"
elif [ -x "$HOME/.cargo/bin/cass" ]; then
    CASS_BIN="$HOME/.cargo/bin/cass"
elif [ -x "$HOME/.local/bin/cass" ]; then
    CASS_BIN="$HOME/.local/bin/cass"
elif [ -x "/usr/local/bin/cass" ]; then
    CASS_BIN="/usr/local/bin/cass"
fi

if [ -n "$CASS_BIN" ]; then
    CASS_VER=$("$CASS_BIN" --version 2>/dev/null | head -1 | awk '{{print $2}}')
    if [ -z "$CASS_VER" ]; then
        # Binary exists but version command failed - treat as not found
        echo "CASS_VERSION=NOT_FOUND"
    else
        echo "CASS_VERSION=$CASS_VER"

        # Get health status (JSON output) - only if version was detected
        if "$CASS_BIN" health --json &>/dev/null; then
            echo "CASS_HEALTH=OK"
            # Try to get session count from stats
            STATS=$("$CASS_BIN" stats --json 2>/dev/null)
            if [ $? -eq 0 ] && [ -n "$STATS" ]; then
                # Extract total conversations from JSON (allow whitespace/newlines)
                SESSIONS=$(echo "$STATS" | tr -d '\n' | sed -n 's/.*"conversations"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
                echo "CASS_SESSIONS=${{SESSIONS:-0}}"
            else
                echo "CASS_SESSIONS=0"
            fi
        else
            echo "CASS_HEALTH=NOT_INDEXED"
        fi
    fi
else
    echo "CASS_VERSION=NOT_FOUND"
fi

# Tool availability - also check ~/.cargo/bin for non-interactive SSH sessions
if command -v cargo &> /dev/null || [ -x "$HOME/.cargo/bin/cargo" ]; then
    echo "HAS_CARGO=1"
else
    echo "HAS_CARGO=0"
fi
if command -v cargo-binstall &> /dev/null || [ -x "$HOME/.cargo/bin/cargo-binstall" ]; then
    echo "HAS_BINSTALL=1"
else
    echo "HAS_BINSTALL=0"
fi
command -v curl &> /dev/null && echo "HAS_CURL=1" || echo "HAS_CURL=0"
command -v wget &> /dev/null && echo "HAS_WGET=1" || echo "HAS_WGET=0"

# Resource info - disk (in KB, converted later)
DISK_KB=$(df -k ~ 2>/dev/null | awk 'NR==2 {{print $4}}')
echo "DISK_AVAIL_KB=${{DISK_KB:-0}}"

# Memory info (Linux)
if [ -f /proc/meminfo ]; then
    MEM_TOTAL=$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{{print $2}}')
    MEM_AVAIL=$(grep MemAvailable /proc/meminfo 2>/dev/null | awk '{{print $2}}')
    echo "MEM_TOTAL_KB=${{MEM_TOTAL:-0}}"
    echo "MEM_AVAIL_KB=${{MEM_AVAIL:-0}}"
else
    # macOS - use sysctl
    if command -v sysctl &> /dev/null; then
        MEM_BYTES=$(sysctl -n hw.memsize 2>/dev/null)
        MEM_KB=$((MEM_BYTES / 1024))
        echo "MEM_TOTAL_KB=${{MEM_KB:-0}}"
        echo "MEM_AVAIL_KB=${{MEM_KB:-0}}"  # macOS doesn't have easy available mem
    fi
fi

# Agent data detection (with sizes and file counts)
PROBE_DIRS=(
{dirs}
)
RECURSIVE_PROBE_DIRS=(
{recursive_dirs}
)

# Recursive source statistics are deliberately path-specific. Parent config
# roots (for example ~/.claude) are presence probes only, while leaf session
# roots (for example ~/.claude/projects) get a bounded metadata scan. This
# keeps setup from traversing caches and from walking an ancestor twice.
is_recursive_probe_path() {{
    local candidate=$1
    local recursive_dir
    for recursive_dir in "${{RECURSIVE_PROBE_DIRS[@]}}"; do
        if [ "$candidate" = "$recursive_dir" ]; then
            return 0
        fi
    done
    return 1
}}

# Return "size_mb|file_count" for session-shaped files under one source root.
# The timeout wraps both find and the stat aggregation. Hosts without either
# helper still report the path's presence, but deliberately skip the recursive
# walk rather than falling back to an unbounded find.
probe_tree_stats() {{
    local root=$1
    local timeout_bin
    local stats
    local status
    local worker_script

    if command -v timeout &> /dev/null; then
        timeout_bin=timeout
    elif command -v gtimeout &> /dev/null; then
        timeout_bin=gtimeout
    else
        return 125
    fi

    # Keep find's NUL-delimited output safe for arbitrary filenames and do
    # portable per-file stat calls in the same timed worker. pipefail carries a
    # find/stat failure through the aggregation instead of letting awk hide it.
    worker_script='
# Keep the worker shell alive after TERM so the timeout kill-after signal reaches
# the complete process group, including descendants that ignore TERM.
trap "" TERM
set -o pipefail
# Use find primaries shared by GNU and BSD implementations. The enclosing
# timeout supplies the traversal bound instead of nonportable depth options.
find "$1" -type f \
    \( -name "*.jsonl" -o -name "*.json" -o -name "*.claude" -o -name "*.db" -o -name "*.md" \) \
    -print0 2>/dev/null |
(
    bytes=0
    count=0
    while IFS= read -r -d "" file; do
        if size=$(stat -c "%s" "$file" 2>/dev/null); then
            :
        elif size=$(stat -f "%z" "$file" 2>/dev/null); then
            :
        else
            continue
        fi
        [ -n "$size" ] || continue
        case "$size" in
            *[!0-9]*) continue ;;
        esac
        bytes=$((bytes + size))
        count=$((count + 1))
    done
    printf "%s|%s\n" "$((bytes / 1048576))" "$count"
)
'

    if stats=$("$timeout_bin" --signal=TERM --kill-after=1s 5s \
        bash -c "$worker_script" probe-tree-stats "$root"); then
        status=0
    else
        status=$?
    fi
    if [ "$status" -ne 0 ]; then
        return "$status"
    fi
    if [ -n "$stats" ]; then
        printf '%s\n' "$stats"
    else
        return 1
    fi
}}

probe_file_size_mb() {{
    local bytes
    if bytes=$(stat -c '%s' "$1" 2>/dev/null); then
        printf '%s\n' "$((bytes / 1048576))"
    elif bytes=$(stat -f '%z' "$1" 2>/dev/null); then
        printf '%s\n' "$((bytes / 1048576))"
    else
        printf '0\n'
    fi
}}

for dir in "${{PROBE_DIRS[@]}}"; do
    # Expand only the leading tilde marker from our static probe list. Do not
    # eval paths: connector-owned paths can contain shell metacharacters.
    case "$dir" in
        "~") expanded_dir="$HOME" ;;
        "~/"*) expanded_dir="$HOME/${{dir#\~/}}" ;;
        *) expanded_dir="$dir" ;;
    esac
    if [ -e "$expanded_dir" ]; then
        if [ -f "$expanded_dir" ]; then
            SIZE=$(probe_file_size_mb "$expanded_dir")
            COUNT=1  # A direct source file is a single session candidate.
        elif is_recursive_probe_path "$dir"; then
            STATS=$(probe_tree_stats "$expanded_dir")
            if [ $? -eq 0 ] && [ -n "$STATS" ]; then
                IFS='|' read -r SIZE COUNT <<< "$STATS"
            else
                # Preserve discovery while making an incomplete estimate
                # explicit and cheap after a timeout or missing helper.
                SIZE=0
                COUNT=0
            fi
        else
            # Ancestor/config roots are intentionally not traversed. Their
            # existence is enough for setup to offer the source path; the
            # connector-specific leaf path carries the actual transcripts.
            SIZE=0
            COUNT=0
        fi
        echo "AGENT_DATA=$dir|${{SIZE:-0}}|${{COUNT:-0}}"
    fi
done

echo "===PROBE_END==="
"#,
        dirs = dirs_str,
        recursive_dirs = recursive_dirs_str
    )
}

/// Probe a single SSH host.
///
/// Runs a comprehensive probe script via SSH to gather system info, cass status,
/// and detected agent data. Uses a single SSH session for efficiency.
///
/// # Arguments
/// * `host` - The discovered SSH host to probe
/// * `timeout_secs` - Connection timeout in seconds
///
/// # Returns
/// A `HostProbeResult` with all gathered information, or error details if probe failed.
pub fn probe_host(host: &DiscoveredHost, timeout_secs: u64) -> HostProbeResult {
    let start = Instant::now();
    let timeout_secs = timeout_secs.max(1);
    let command_timeout = Duration::from_secs(timeout_secs);

    // Build SSH command with strict host key verification.
    // Security-first: do not auto-trust unknown hosts during probing.
    // Use the host alias directly (SSH config handles Port, User, IdentityFile, ProxyJump, etc.)
    let probe_script = build_probe_script();
    let child_stdin = match file_backed_child_stdin(probe_script.as_bytes()) {
        Ok(stdin) => stdin,
        Err(e) => {
            return HostProbeResult::unreachable(
                &host.name,
                format!("Failed to prepare SSH probe input: {e}"),
            );
        }
    };
    let mut cmd = Command::new("ssh");
    cmd.args(strict_ssh_cli_tokens(timeout_secs))
        .arg("--")
        .arg(&host.name)
        .arg("bash -s")
        .stdin(child_stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut cmd);

    // Spawn the process; the probe script reaches bash through the
    // file-backed stdin above (no pipe write from cass — ztlan/gh#358).
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HostProbeResult::unreachable(
                &host.name,
                format!("Failed to execute ssh: {}", e),
            );
        }
    };

    // Wait for completion
    let output = match wait_for_child_output_with_timeout(child, command_timeout) {
        Ok(Some(o)) => o,
        Ok(None) => {
            return HostProbeResult::unreachable(
                &host.name,
                format!("Connection timed out after {timeout_secs} seconds"),
            );
        }
        Err(e) => {
            return HostProbeResult::unreachable(&host.name, format!("SSH command failed: {}", e));
        }
    };

    let connection_time_ms = start.elapsed().as_millis() as u64;

    // Check for SSH failures
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_msg = if stderr.contains("Connection refused") {
            "Connection refused".to_string()
        } else if stderr.contains("Connection timed out") || stderr.contains("timed out") {
            "Connection timed out".to_string()
        } else if stderr.contains("Permission denied") {
            "Permission denied (key not loaded in ssh-agent?)".to_string()
        } else if is_host_key_verification_failure(&stderr) {
            host_key_verification_error(&host.name)
        } else if stderr.contains("No route to host") {
            "No route to host".to_string()
        } else {
            format!("SSH failed: {}", stderr.trim())
        };

        return HostProbeResult::unreachable(&host.name, error_msg);
    }
    // Parse successful output
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_probe_output(&host.name, &stdout, connection_time_ms)
}

/// Parse the probe script output into a HostProbeResult.
fn parse_probe_output(host_name: &str, output: &str, connection_time_ms: u64) -> HostProbeResult {
    let mut values: HashMap<String, String> = HashMap::new();
    let mut agent_data: Vec<(String, u64, u64)> = Vec::new(); // (path, size_mb, count)

    // Parse only key=value pairs emitted by the probe script itself. SSH login
    // banners, forced-command wrappers, or shell noise can appear before or
    // after the markers and must not override the measured values.
    let mut inside_probe = false;
    let mut saw_start = false;
    let mut saw_end = false;
    for line in output.lines() {
        let line = line.trim();
        if line == "===PROBE_START===" {
            if saw_start {
                return HostProbeResult::unreachable(host_name, "Probe script output malformed");
            }
            saw_start = true;
            inside_probe = true;
            continue;
        }
        if line == "===PROBE_END===" {
            if !inside_probe {
                return HostProbeResult::unreachable(host_name, "Probe script output malformed");
            }
            saw_end = true;
            break;
        }
        if !inside_probe || line.is_empty() || line.starts_with("===") {
            continue;
        }

        if line.starts_with("AGENT_DATA=") {
            // Special handling for agent data: AGENT_DATA=path|size|count
            if let Some(data) = line.strip_prefix("AGENT_DATA=") {
                // Use rsplitn to handle paths containing pipes (parse from right)
                // Yields: count, size, path
                let parts: Vec<&str> = data.rsplitn(3, '|').collect();
                if parts.len() == 3 {
                    let count = parts[0].parse().unwrap_or(0);
                    let size = parts[1].parse().unwrap_or(0);
                    let path = parts[2].to_string();
                    agent_data.push((path, size, count));
                }
            }
        } else if let Some((key, value)) = line.split_once('=') {
            values.insert(key.to_string(), value.to_string());
        }
    }

    if !saw_start || !saw_end {
        return HostProbeResult::unreachable(host_name, "Probe script output malformed");
    }

    // Build CassStatus
    let cass_status = if let Some(version) = values.get("CASS_VERSION") {
        if version == "NOT_FOUND" {
            CassStatus::NotFound
        } else {
            let health = values.get("CASS_HEALTH").map(|s| s.as_str());
            if health == Some("OK") {
                let sessions = values
                    .get("CASS_SESSIONS")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                CassStatus::Indexed {
                    version: version.clone(),
                    session_count: sessions,
                    last_indexed: None,
                }
            } else {
                CassStatus::InstalledNotIndexed {
                    version: version.clone(),
                }
            }
        }
    } else {
        CassStatus::Unknown
    };

    // Build SystemInfo
    let system_info = values.get("OS").map(|os| SystemInfo {
        os: os.clone(),
        arch: values.get("ARCH").cloned().unwrap_or_default(),
        distro: values.get("DISTRO").cloned(),
        has_cargo: values.get("HAS_CARGO").map(|v| v == "1").unwrap_or(false),
        has_cargo_binstall: values
            .get("HAS_BINSTALL")
            .map(|v| v == "1")
            .unwrap_or(false),
        has_curl: values.get("HAS_CURL").map(|v| v == "1").unwrap_or(false),
        has_wget: values.get("HAS_WGET").map(|v| v == "1").unwrap_or(false),
        remote_home: values.get("HOME").cloned().unwrap_or_default(),
        machine_id: values.get("MACHINE_ID").cloned().filter(|s| !s.is_empty()),
    });

    // Build ResourceInfo
    let resources = {
        let disk_kb = values
            .get("DISK_AVAIL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mem_total_kb = values
            .get("MEM_TOTAL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mem_avail_kb = values
            .get("MEM_AVAIL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if disk_kb > 0 || mem_total_kb > 0 {
            let disk_mb = disk_kb / 1024;
            let mem_total_mb = mem_total_kb / 1024;
            let mem_avail_mb = mem_avail_kb / 1024;

            Some(ResourceInfo {
                disk_available_mb: disk_mb,
                memory_total_mb: mem_total_mb,
                memory_available_mb: mem_avail_mb,
                can_compile: disk_mb >= ResourceInfo::MIN_DISK_MB
                    && mem_total_mb >= ResourceInfo::MIN_MEMORY_MB,
            })
        } else {
            None
        }
    };

    // Build DetectedAgents
    let detected_agents: Vec<DetectedAgent> = agent_data
        .into_iter()
        .map(|(path, size_mb, count)| {
            let agent_type = infer_agent_type(&path);
            DetectedAgent {
                agent_type,
                path,
                estimated_sessions: Some(count),
                estimated_size_mb: Some(size_mb),
            }
        })
        .collect();

    HostProbeResult {
        host_name: host_name.to_string(),
        reachable: true,
        connection_time_ms,
        cass_status,
        detected_agents,
        system_info,
        resources,
        error: None,
    }
}

/// Infer agent type from path.
///
/// Note: More specific patterns must be checked first (e.g., `saoudrizwan.claude-dev`
/// contains `claude` so Cline must be checked before Claude Code).
fn infer_agent_type(path: &str) -> String {
    // Check Cline first - it contains "claude-dev" which could match ".claude"
    if path.contains("saoudrizwan.claude-dev") || path.contains("rooveterinaryinc.roo-cline") {
        "cline".to_string()
    } else if path.contains(".claude") {
        "claude_code".to_string()
    } else if path.contains(".codex") {
        "codex".to_string()
    } else if path.contains(".cursor") || path.contains("Cursor") {
        "cursor".to_string()
    } else if path.contains("antigravity") {
        // Antigravity lives under ~/.gemini/antigravity/ (IDE) and
        // ~/.gemini/antigravity-cli/ (agy CLI), which also contain ".gemini"
        // — so it MUST be matched before the gemini branch below, or those
        // roots would be mislabeled as legacy Gemini CLI.
        "antigravity".to_string()
    } else if path.contains(".gemini") {
        "gemini".to_string()
    } else if path.contains("/.pi/") || path.ends_with("/.pi") {
        "pi_agent".to_string()
    } else if path.contains("/.prime/agent/") || path.ends_with("/.prime/agent") {
        "prime_agent".to_string()
    } else if path.contains("/.omp/")
        || path.ends_with("/.omp")
        || path.contains("/omp/sessions")
        || path.contains("/omp/profiles/")
        || path.ends_with("/omp")
    {
        "omp".to_string()
    } else if path.contains(".aider") {
        "aider".to_string()
    } else if path.contains("opencode") {
        "opencode".to_string()
    } else if path.contains(".goose") {
        "goose".to_string()
    } else if path.contains("copilot-chat")
        || path.contains("gh-copilot")
        || path.contains("gh/copilot")
    {
        "copilot".to_string()
    } else if path.contains(".continue") {
        "continue".to_string()
    } else if path.contains("sourcegraph.amp") || path.contains("/amp/") || path.ends_with("/amp") {
        "amp".to_string()
    } else if path.contains(".clawdbot") {
        "clawdbot".to_string()
    } else if path.contains(".factory") {
        "factory".to_string()
    } else if path.contains(".vibe") {
        "vibe".to_string()
    } else if path.contains(".windsurf") {
        "windsurf".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Probe multiple hosts in parallel.
///
/// Uses rayon's parallel iterator to probe hosts concurrently, calling the
/// progress callback as each probe completes.
///
/// # Arguments
/// * `hosts` - Slice of discovered hosts to probe
/// * `timeout_secs` - Connection timeout per host
/// * `on_progress` - Callback called after each host completes: (completed, total, host_name)
///
/// # Returns
/// Vector of probe results for all hosts.
pub fn probe_hosts_parallel<F>(
    hosts: &[DiscoveredHost],
    timeout_secs: u64,
    on_progress: F,
) -> Vec<HostProbeResult>
where
    F: Fn(usize, usize, &str) + Send + Sync,
{
    use rayon::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let total = hosts.len();
    let completed = Arc::new(AtomicUsize::new(0));
    let on_progress = Arc::new(on_progress);

    // Use rayon for true parallel execution
    hosts
        .par_iter()
        .map(|host| {
            let result = probe_host(host, timeout_secs);

            let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
            on_progress(done, total, &host.name);

            result
        })
        .collect()
}

/// Cache for probe results to avoid repeated probing.
///
/// Note: Use `ProbeCache::new(ttl_secs)` to create a cache. The `Default`
/// implementation uses a 5-minute TTL.
#[derive(Debug)]
pub struct ProbeCache {
    results: HashMap<String, (HostProbeResult, std::time::Instant)>,
    ttl_secs: u64,
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new(300) // 5-minute default TTL
    }
}

impl ProbeCache {
    /// Create a new cache with the specified TTL in seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            results: HashMap::new(),
            ttl_secs,
        }
    }

    /// Get a cached result if still valid.
    pub fn get(&self, host_name: &str) -> Option<&HostProbeResult> {
        self.results.get(host_name).and_then(|(result, ts)| {
            if ts.elapsed().as_secs() < self.ttl_secs {
                Some(result)
            } else {
                None
            }
        })
    }

    /// Insert a result into the cache.
    pub fn insert(&mut self, result: HostProbeResult) {
        self.results.insert(
            result.host_name.clone(),
            (result, std::time::Instant::now()),
        );
    }

    /// Clear expired entries.
    pub fn clear_expired(&mut self) {
        self.results
            .retain(|_, (_, ts)| ts.elapsed().as_secs() < self.ttl_secs);
    }
}

/// Deduplicate probe results that point to the same physical machine.
///
/// Multiple SSH aliases may point to the same machine. This function identifies
/// duplicates using the machine_id from the probe and keeps only one entry per
/// physical machine.
///
/// # Selection criteria (when duplicates found)
/// 1. Prefer hosts with cass already installed
/// 2. Prefer hosts with more sessions indexed
/// 3. Otherwise, keep the first one alphabetically
///
/// # Returns
/// A tuple of (deduplicated results, merged aliases map).
/// The merged map contains: kept_host_name -> vec![merged_alias_names]
pub fn deduplicate_probe_results(
    results: Vec<HostProbeResult>,
) -> (Vec<HostProbeResult>, HashMap<String, Vec<String>>) {
    // Group by machine_id (skip hosts without machine_id - can't dedupe them)
    let mut by_machine_id: HashMap<String, Vec<HostProbeResult>> = HashMap::new();
    let mut no_machine_id: Vec<HostProbeResult> = Vec::new();

    for result in results {
        if let Some(ref machine_id) = result
            .system_info
            .as_ref()
            .and_then(|s| s.machine_id.clone())
        {
            by_machine_id
                .entry(machine_id.clone())
                .or_default()
                .push(result);
        } else {
            no_machine_id.push(result);
        }
    }

    let mut deduplicated: Vec<HostProbeResult> = Vec::new();
    let mut merged_aliases: HashMap<String, Vec<String>> = HashMap::new();

    // Process groups with machine_id
    for (_machine_id, mut group) in by_machine_id {
        if group.len() == 1 {
            deduplicated.push(group.remove(0));
        } else {
            // Multiple aliases for same machine - pick the best one
            group.sort_by(|a, b| {
                // 1. Prefer installed cass
                let a_installed = a.cass_status.is_installed();
                let b_installed = b.cass_status.is_installed();
                if a_installed != b_installed {
                    return b_installed.cmp(&a_installed);
                }

                // 2. Prefer more sessions
                let a_sessions = match &a.cass_status {
                    CassStatus::Indexed { session_count, .. } => *session_count,
                    _ => 0,
                };
                let b_sessions = match &b.cass_status {
                    CassStatus::Indexed { session_count, .. } => *session_count,
                    _ => 0,
                };
                if a_sessions != b_sessions {
                    return b_sessions.cmp(&a_sessions);
                }

                // 3. Alphabetically by name
                a.host_name.cmp(&b.host_name)
            });

            // Keep the first (best) one, record others as merged
            let kept = group.remove(0);
            let merged: Vec<String> = group.into_iter().map(|h| h.host_name).collect();

            if !merged.is_empty() {
                merged_aliases.insert(kept.host_name.clone(), merged);
            }
            deduplicated.push(kept);
        }
    }

    // Add back hosts without machine_id
    deduplicated.extend(no_machine_id);

    // Sort final list by name for consistent ordering
    deduplicated.sort_by(|a, b| a.host_name.cmp(&b.host_name));

    (deduplicated, merged_aliases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cass_status_is_installed() {
        assert!(
            CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None
            }
            .is_installed()
        );

        assert!(
            CassStatus::InstalledNotIndexed {
                version: "0.1.50".into()
            }
            .is_installed()
        );

        assert!(!CassStatus::NotFound.is_installed());
        assert!(!CassStatus::Unknown.is_installed());
    }

    #[test]
    fn test_cass_status_version() {
        assert_eq!(
            CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 0,
                last_indexed: None
            }
            .version(),
            Some("0.1.50")
        );

        assert_eq!(
            CassStatus::InstalledNotIndexed {
                version: "0.1.49".into()
            }
            .version(),
            Some("0.1.49")
        );

        assert_eq!(CassStatus::NotFound.version(), None);
    }

    #[test]
    fn test_infer_agent_type() {
        assert_eq!(infer_agent_type("~/.claude/projects"), "claude_code");
        assert_eq!(infer_agent_type("~/.codex/sessions"), "codex");
        assert_eq!(infer_agent_type("~/.cursor"), "cursor");
        assert_eq!(infer_agent_type("~/.gemini/tmp"), "gemini");
        // Antigravity (agy) shares the ~/.gemini parent but must NOT be
        // misclassified as the legacy Gemini CLI.
        assert_eq!(
            infer_agent_type("~/.gemini/antigravity-cli/conversations"),
            "antigravity"
        );
        assert_eq!(
            infer_agent_type("~/.gemini/antigravity-cli/brain/abc/.system_generated/logs"),
            "antigravity"
        );
        // The Antigravity IDE store (#454) shares the same parent.
        assert_eq!(infer_agent_type("~/.gemini/antigravity"), "antigravity");
        assert_eq!(
            infer_agent_type("~/.gemini/antigravity/brain/abc/.system_generated/logs"),
            "antigravity"
        );
        assert_eq!(
            infer_agent_type("~/.config/Code/User/globalStorage/saoudrizwan.claude-dev"),
            "cline"
        );
        assert_eq!(
            infer_agent_type("~/.config/Code/User/globalStorage/github.copilot-chat"),
            "copilot"
        );
        assert_eq!(infer_agent_type("~/.config/gh-copilot"), "copilot");
        assert_eq!(infer_agent_type("/some/random/path"), "unknown");
    }

    #[test]
    fn test_parse_probe_output_success() {
        let output = r#"
===PROBE_START===
OS=linux
ARCH=x86_64
HOME=/home/user
DISTRO=Ubuntu 22.04
CASS_VERSION=0.1.50
CASS_HEALTH=OK
CASS_SESSIONS=1234
HAS_CARGO=1
HAS_BINSTALL=0
HAS_CURL=1
HAS_WGET=1
DISK_AVAIL_KB=52428800
MEM_TOTAL_KB=16777216
MEM_AVAIL_KB=8388608
AGENT_DATA=~/.claude/projects|150|42
AGENT_DATA=~/.codex/sessions|50|10
===PROBE_END===
"#;

        let result = parse_probe_output("test-host", output, 100);

        assert!(result.reachable);
        assert_eq!(result.host_name, "test-host");
        assert_eq!(result.connection_time_ms, 100);

        // Check cass status
        assert!(
            matches!(&result.cass_status, CassStatus::Indexed { .. }),
            "expected Indexed status"
        );
        if let CassStatus::Indexed {
            version,
            session_count,
            ..
        } = &result.cass_status
        {
            assert_eq!(version, "0.1.50");
            assert_eq!(*session_count, 1234);
        }

        // Check system info
        let sys = result.system_info.as_ref().unwrap();
        assert_eq!(sys.os, "linux");
        assert_eq!(sys.arch, "x86_64");
        assert_eq!(sys.distro, Some("Ubuntu 22.04".into()));
        assert!(sys.has_cargo);
        assert!(!sys.has_cargo_binstall);
        assert!(sys.has_curl);

        // Check resources
        let res = result.resources.as_ref().unwrap();
        assert_eq!(res.disk_available_mb, 51200); // 52428800 / 1024
        assert_eq!(res.memory_total_mb, 16384); // 16777216 / 1024
        assert!(res.can_compile);

        // Check detected agents
        assert_eq!(result.detected_agents.len(), 2);
        assert_eq!(result.detected_agents[0].agent_type, "claude_code");
        assert_eq!(result.detected_agents[0].estimated_sessions, Some(42));
        assert_eq!(result.detected_agents[1].agent_type, "codex");
    }

    #[test]
    fn test_parse_probe_output_ignores_noise_outside_markers() {
        let output = r#"
CASS_VERSION=NOT_FOUND
AGENT_DATA=/tmp/outside-before|999|999
===PROBE_START===
OS=linux
ARCH=x86_64
HOME=/home/user
CASS_VERSION=0.4.2
CASS_HEALTH=OK
CASS_SESSIONS=7
HAS_CARGO=1
HAS_BINSTALL=0
HAS_CURL=1
HAS_WGET=1
DISK_AVAIL_KB=2048000
MEM_TOTAL_KB=4096000
MEM_AVAIL_KB=1024000
===PROBE_END===
CASS_VERSION=NOT_FOUND
AGENT_DATA=/tmp/outside-after|999|999
"#;

        let result = parse_probe_output("noisy-host", output, 42);

        assert!(result.reachable);
        assert!(result.detected_agents.is_empty());
        assert!(matches!(
            result.cass_status,
            CassStatus::Indexed {
                ref version,
                session_count: 7,
                ..
            } if version == "0.4.2"
        ));
    }

    #[test]
    fn test_parse_probe_output_cass_not_found() {
        let output = r#"
===PROBE_START===
OS=darwin
ARCH=arm64
HOME=/Users/user
CASS_VERSION=NOT_FOUND
HAS_CARGO=0
HAS_BINSTALL=0
HAS_CURL=1
HAS_WGET=0
DISK_AVAIL_KB=10240000
MEM_TOTAL_KB=8388608
MEM_AVAIL_KB=4194304
===PROBE_END===
"#;

        let result = parse_probe_output("mac-host", output, 50);

        assert!(result.reachable);
        assert!(matches!(result.cass_status, CassStatus::NotFound));

        let sys = result.system_info.as_ref().unwrap();
        assert_eq!(sys.os, "darwin");
        assert_eq!(sys.arch, "arm64");
        assert!(!sys.has_cargo);
    }

    #[test]
    fn test_parse_probe_output_malformed() {
        let output = "random garbage";
        let result = parse_probe_output("bad-host", output, 0);

        assert!(!result.reachable);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_parse_probe_output_rejects_out_of_order_markers() {
        let output = r#"
===PROBE_END===
===PROBE_START===
OS=linux
CASS_VERSION=0.4.2
"#;
        let result = parse_probe_output("bad-host", output, 0);

        assert!(!result.reachable);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_host_probe_result_unreachable() {
        let result = HostProbeResult::unreachable("test", "Connection refused");

        assert!(!result.reachable);
        assert_eq!(result.error, Some("Connection refused".into()));
        assert!(!result.has_cass());
        assert!(!result.has_agent_data());
    }

    #[test]
    fn test_probe_cache() {
        let mut cache = ProbeCache::new(300); // 5 minute TTL

        let result = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 100,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        cache.insert(result);

        assert!(cache.get("test").is_some());
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_resource_info_can_compile() {
        let good = ResourceInfo {
            disk_available_mb: 2000,
            memory_total_mb: 4000,
            memory_available_mb: 2000,
            can_compile: true,
        };
        assert!(good.can_compile);

        let low_disk = ResourceInfo {
            disk_available_mb: 500,
            memory_total_mb: 4000,
            memory_available_mb: 2000,
            can_compile: false,
        };
        assert!(!low_disk.can_compile);
    }

    // =========================================================================
    // Real system probe tests — run PROBE_SCRIPT locally without SSH
    // =========================================================================

    /// Execute a probe script on the local system via bash, returning stdout.
    fn run_probe_script_with_home(script: &str, home: Option<&std::path::Path>) -> String {
        use std::io::Write;
        let mut cmd = Command::new("bash");
        cmd.arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(home) = home {
            cmd.env("HOME", home);
        } else if dotenvy::var("HOME").is_err()
            && let Some(dirs) = directories::BaseDirs::new()
        {
            // Ensure HOME is set for the probe script (may not be set in some test environments).
            cmd.env("HOME", dirs.home_dir());
        }
        let mut child = cmd.spawn().expect("bash should be available");
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(script.as_bytes())
                .expect("write probe script");
        }
        let output = child
            .wait_with_output()
            .expect("probe script should finish");
        assert!(
            output.status.success(),
            "probe script failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    #[cfg(unix)]
    fn run_probe_script_with_path_value(
        script: &str,
        home: &std::path::Path,
        path_value: &str,
        timeout: Duration,
    ) -> Option<std::process::Output> {
        use std::io::Write;

        let mut cmd = Command::new("bash");
        cmd.arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", home)
            .env("PATH", path_value);
        configure_child_process_group(&mut cmd);

        let mut child = cmd.spawn().expect("bash should be available");
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(script.as_bytes())
                .expect("write probe script");
        }
        wait_for_child_output_with_timeout(child, timeout)
            .expect("probe script should finish or hit its deadline")
    }
    #[cfg(unix)]
    fn write_test_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).expect("write test executable");
        let mut permissions = std::fs::metadata(path)
            .expect("test executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("set test executable permissions");
    }

    #[cfg(unix)]
    fn link_system_tools(tool_dir: &std::path::Path, names: &[&str]) {
        let current_path = std::env::var_os("PATH").expect("test PATH");
        for name in names {
            let source = std::env::split_paths(&current_path)
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| panic!("missing system tool {name}"));
            std::os::unix::fs::symlink(source, tool_dir.join(name))
                .unwrap_or_else(|error| panic!("link system tool {name}: {error}"));
        }
    }

    /// Execute PROBE_SCRIPT on the local system via bash, returning stdout.
    fn run_probe_script_locally() -> String {
        run_probe_script_with_home(&build_probe_script(), None)
    }

    #[test]
    fn shell_single_quote_arg_quotes_shell_metacharacters() {
        assert_eq!(shell_single_quote_arg("plain/path"), "'plain/path'");
        assert_eq!(shell_single_quote_arg("can't"), "'can'\\''t'");
        assert_eq!(
            shell_single_quote_arg("$(touch /tmp/nope); `whoami`"),
            "'$(touch /tmp/nope); `whoami`'"
        );
    }

    #[test]
    fn probe_script_uses_literal_array_without_eval() {
        let script = build_probe_script();
        assert!(script.contains("PROBE_DIRS=("));
        assert!(script.contains("for dir in \"${PROBE_DIRS[@]}\""));
        assert!(script.contains("expanded_dir=\"$HOME/${dir#\\~/}\""));
        assert!(
            !script.contains("eval echo"),
            "probe paths must not be expanded through eval"
        );
    }

    #[test]
    fn recursive_probe_dirs_walks_only_component_descendant_leaves() {
        let dirs = vec![
            "~/.claude".to_string(),
            "~/.claude/projects".to_string(),
            "~/.claude-work".to_string(),
            "~/.codex/sessions".to_string(),
        ];

        assert_eq!(
            recursive_probe_dirs(&dirs),
            vec![
                "~/.claude/projects".to_string(),
                "~/.claude-work".to_string(),
                "~/.codex/sessions".to_string(),
            ]
        );
        assert!(is_path_ancestor("~/.claude", "~/.claude/projects"));
        assert!(!is_path_ancestor("~/.claude", "~/.claude-work"));
        assert!(!is_path_ancestor("~/.claude", "~/.claude"));
    }

    #[test]
    #[cfg(not(windows))]
    fn probe_script_marks_parent_as_presence_only_and_counts_leaf_sessions() {
        let home = tempfile::tempdir().expect("temp home");
        let parent = home.path().join(".claude");
        let leaf = parent.join("projects");
        std::fs::create_dir_all(parent.join("cache")).expect("create parent cache");
        std::fs::create_dir_all(&leaf).expect("create leaf");
        std::fs::write(parent.join("cache").join("unrelated.json"), b"{}")
            .expect("write parent-only file");
        std::fs::write(leaf.join("session.jsonl"), b"turn\n").expect("write leaf session");

        let dirs = vec!["~/.claude".to_string(), "~/.claude/projects".to_string()];
        let recursive_dirs = recursive_probe_dirs(&dirs);
        let script = build_probe_script_for_dir_modes(&dirs, &recursive_dirs);
        let output = run_probe_script_with_home(&script, Some(home.path()));

        assert!(
            output.contains("AGENT_DATA=~/.claude|0|0"),
            "parent config root must be presence-only: {output}"
        );
        assert!(
            output.contains("AGENT_DATA=~/.claude/projects|0|1"),
            "leaf source must count its transcript without the parent cache: {output}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn probe_script_hard_kills_term_ignoring_find_and_classifies_timeout() {
        struct PidCleanup(Vec<i32>);

        impl Drop for PidCleanup {
            fn drop(&mut self) {
                for pid in &self.0 {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
            }
        }

        let home = tempfile::tempdir().expect("temp home");
        let leaf = home.path().join(".claude/projects");
        std::fs::create_dir_all(&leaf).expect("create leaf");

        let fake_tools = tempfile::tempdir().expect("fake tools");
        let find_path = fake_tools.path().join("find");
        let fifo_path = find_path.with_extension("pipe");
        let pid_path = find_path.with_extension("pid");
        let mkfifo = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo should run");
        assert!(mkfifo.success(), "mkfifo should succeed");
        write_test_executable(
            &find_path,
            r#"#!/bin/bash
exec 9<> "$0.pipe"
(
    trap '' TERM
    while :; do
        read -r -t 1 _ <&9 || true
    done
) &
worker=$!
printf '%s\n%s\n' "$$" "$worker" > "$0.pid"
trap '' TERM
while :; do
    read -r -t 1 _ <&9 || true
done
"#,
        );

        let script = build_probe_script_for_dirs(&["~/.claude/projects".to_string()]);
        let path_value = format!("{}:/usr/bin:/bin", fake_tools.path().display());
        let output = run_probe_script_with_path_value(
            &script,
            home.path(),
            &path_value,
            Duration::from_secs(12),
        );
        let pids = std::fs::read_to_string(&pid_path)
            .expect("fake find should record its PIDs")
            .lines()
            .map(|line| line.parse::<i32>().expect("valid fake find PID"))
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 2, "fake find should record parent and child");
        let _cleanup = PidCleanup(pids.clone());

        let mut live_pids = pids.clone();
        for _ in 0..30 {
            live_pids = pids
                .iter()
                .copied()
                .filter(|pid| {
                    Command::new("kill")
                        .args(["-0", &pid.to_string()])
                        .status()
                        .is_ok_and(|status| status.success())
                })
                .collect();
            if live_pids.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let output = output.expect("outer probe shell should finish");
        assert!(output.status.success(), "probe shell failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("AGENT_DATA=~/.claude/projects|0|0"),
            "timeout must classify the recursive estimate as incomplete: {stdout}"
        );
        assert!(
            live_pids.is_empty(),
            "timeout must kill find and its child; leaked PIDs: {live_pids:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn probe_script_uses_bsd_stat_and_gtimeout_and_preserves_find_failure_status() {
        let home = tempfile::tempdir().expect("temp home");
        let leaf = home.path().join(".codex/sessions");
        std::fs::create_dir_all(&leaf).expect("create leaf");
        let weird_name = r#"session space;$(touch "$HOME/SHOULD_NOT_EXIST").jsonl"#;
        let weird_path = leaf.join(weird_name);
        if let Some(parent) = weird_path.parent() {
            std::fs::create_dir_all(parent).expect("create literal path parent");
        }
        std::fs::write(&weird_path, b"session").expect("write literal session");

        let fake_tools = tempfile::tempdir().expect("fake tools");
        let tool_bin = tempfile::tempdir().expect("system tool links");
        link_system_tools(
            tool_bin.path(),
            &[
                "bash", "uname", "tr", "cat", "df", "awk", "grep", "sed", "head",
            ],
        );

        let find_path = fake_tools.path().join("find");
        write_test_executable(
            &find_path,
            r##"#!/bin/bash
if [ -e "$0.fail" ]; then
    exit 73
fi
printf '%s\n' "$@" > "$0.args"
for arg in "$@"; do
    [ "$arg" = "-printf" ] && exit 97
done
file="$1/session space;\$(touch \"\$HOME/SHOULD_NOT_EXIST\").jsonl"
printf '%s\0' "$file"
"##,
        );

        let stat_path = fake_tools.path().join("stat");
        write_test_executable(
            &stat_path,
            r#"#!/bin/bash
if [ "$1" = "-c" ]; then
    exit 2
fi
if [ "$1" = "-f" ] && [ "$2" = "%z" ]; then
    printf '%s\n' "$3" > "$0.path"
    printf '2097152\n'
    exit 0
fi
exit 3
"#,
        );

        let gtimeout_path = fake_tools.path().join("gtimeout");
        write_test_executable(
            &gtimeout_path,
            r#"#!/bin/bash
printf '%s\n' "$@" > "$0.args"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --signal=*|--kill-after=*) shift ;;
        *) break ;;
    esac
done
shift
"$@"
status=$?
printf '%s\n' "$status" > "$0.status"
exit "$status"
"#,
        );

        let path_value = format!(
            "{}:{}",
            fake_tools.path().display(),
            tool_bin.path().display()
        );
        let script = build_probe_script_for_dirs(&["~/.codex/sessions".to_string()]);
        let output = run_probe_script_with_path_value(
            &script,
            home.path(),
            &path_value,
            Duration::from_secs(8),
        )
        .expect("portable probe shell should finish");
        assert!(output.status.success(), "portable probe shell failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("AGENT_DATA=~/.codex/sessions|2|1"),
            "BSD stat fallback should count the literal session: {stdout}"
        );

        let find_args =
            std::fs::read_to_string(find_path.with_extension("args")).expect("find args");
        assert!(find_args.contains("-print0"));
        assert!(!find_args.contains("-printf"));
        let timeout_args =
            std::fs::read_to_string(gtimeout_path.with_extension("args")).expect("gtimeout args");
        assert!(timeout_args.contains("--signal=TERM"));
        assert!(timeout_args.contains("--kill-after=1s"));
        assert!(timeout_args.contains("5s"));
        assert_eq!(
            std::fs::read_to_string(gtimeout_path.with_extension("status"))
                .expect("gtimeout status")
                .trim(),
            "0"
        );
        assert_eq!(
            std::fs::read_to_string(stat_path.with_extension("path"))
                .expect("stat path")
                .trim_end(),
            weird_path.to_string_lossy()
        );
        assert!(
            !home.path().join("SHOULD_NOT_EXIST").exists(),
            "literal metacharacters from find must not execute"
        );

        std::fs::write(find_path.with_extension("fail"), b"fail").expect("enable find failure");
        let failure = run_probe_script_with_path_value(
            &script,
            home.path(),
            &path_value,
            Duration::from_secs(8),
        )
        .expect("failed find probe shell should finish");
        assert!(
            failure.status.success(),
            "outer probe should preserve discovery"
        );
        let failure_stdout = String::from_utf8_lossy(&failure.stdout);
        assert!(
            failure_stdout.contains("AGENT_DATA=~/.codex/sessions|0|0"),
            "find failure must classify the estimate as incomplete: {failure_stdout}"
        );
        assert_eq!(
            std::fs::read_to_string(gtimeout_path.with_extension("status"))
                .expect("failed gtimeout status")
                .trim(),
            "73",
            "find's nonzero status must survive the aggregation pipeline"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn probe_script_treats_special_probe_paths_as_literals() {
        let home = tempfile::tempdir().expect("temp home");
        let relative_path =
            "Library/Application Support/Codex$(touch \"$HOME/SHOULD_NOT_EXIST\");can't";
        std::fs::create_dir_all(home.path().join(relative_path)).expect("create special path");

        let probe_path = format!("~/{relative_path}");
        let script = build_probe_script_for_dirs(std::slice::from_ref(&probe_path));
        let output = run_probe_script_with_home(&script, Some(home.path()));

        assert!(
            output.contains(&format!("AGENT_DATA={probe_path}|")),
            "special probe path should be reported literally: {output}"
        );
        assert!(
            !home.path().join("SHOULD_NOT_EXIST").exists(),
            "probe path interpolation must not execute command substitutions"
        );

        let result = parse_probe_output("localhost", &output, 0);
        assert!(
            result
                .detected_agents
                .iter()
                .any(|agent| agent.path == probe_path),
            "parsed agent data should preserve literal path"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_script_produces_valid_markers() {
        let output = run_probe_script_locally();
        assert!(
            output.contains("===PROBE_START==="),
            "missing PROBE_START marker"
        );
        assert!(
            output.contains("===PROBE_END==="),
            "missing PROBE_END marker"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_script_parses_into_reachable_result() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        assert!(
            result.reachable,
            "local probe should be reachable: {:?}",
            result.error
        );
        assert!(result.system_info.is_some(), "should have system info");
        assert!(result.resources.is_some(), "should have resource info");
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_system_info_has_valid_os() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        assert!(
            sys.os == "linux" || sys.os == "darwin",
            "OS should be linux or darwin, got: {}",
            sys.os
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_system_info_has_valid_arch() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        let valid_archs = [
            "x86_64", "aarch64", "arm64", "armv7l", "i686", "s390x", "ppc64le",
        ];
        assert!(
            valid_archs.contains(&sys.arch.as_str()),
            "arch should be a known value, got: {}",
            sys.arch
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_system_info_has_nonempty_home() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        assert!(!sys.remote_home.is_empty(), "home should not be empty");
        assert!(
            sys.remote_home.starts_with('/'),
            "home should be absolute: {}",
            sys.remote_home
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_resources_have_nonzero_disk() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(res.disk_available_mb > 0, "disk_available_mb should be > 0");
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_resources_have_nonzero_memory() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(res.memory_total_mb > 0, "memory_total_mb should be > 0");
        assert!(
            res.memory_available_mb > 0,
            "memory_available_mb should be > 0"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_resources_memory_invariant() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(
            res.memory_available_mb <= res.memory_total_mb,
            "available memory ({}) should not exceed total ({})",
            res.memory_available_mb,
            res.memory_total_mb
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_resources_can_compile_reflects_thresholds() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        let expected = res.disk_available_mb >= ResourceInfo::MIN_DISK_MB
            && res.memory_total_mb >= ResourceInfo::MIN_MEMORY_MB;
        assert_eq!(
            res.can_compile, expected,
            "can_compile should match threshold check: disk={}MB mem={}MB",
            res.disk_available_mb, res.memory_total_mb
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_tool_detection_is_consistent() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        // If cargo-binstall is available, cargo must also be available
        if sys.has_cargo_binstall {
            assert!(sys.has_cargo, "binstall requires cargo");
        }
        // At least one download tool should exist on any modern system
        assert!(
            sys.has_curl || sys.has_wget,
            "system should have at least curl or wget"
        );
    }

    #[test]
    fn probe_script_contains_all_franken_agent_detection_paths() {
        let script = build_probe_script();
        // Verify key agent paths from franken_agent_detection are present
        assert!(script.contains("~/.claude"), "missing claude paths");
        assert!(script.contains("~/.codex/sessions"), "missing codex path");
        assert!(script.contains("~/.gemini"), "missing gemini paths");
        assert!(script.contains("~/.goose/sessions"), "missing goose path");
        assert!(
            script.contains("~/.continue/sessions"),
            "missing continue path"
        );
        assert!(script.contains("~/.aider"), "missing aider path");
        assert!(
            script.contains("saoudrizwan.claude-dev"),
            "missing cline path"
        );
        assert!(script.contains("copilot-chat"), "missing copilot path");
        assert!(script.contains("~/.windsurf"), "missing windsurf path");
        assert!(script.contains("~/.factory"), "missing factory path");
        assert!(script.contains("~/.clawdbot"), "missing clawdbot path");
        assert!(script.contains("~/.vibe"), "missing vibe path");
        assert!(script.contains("sourcegraph.amp"), "missing amp path");
        assert!(script.contains("~/.omp/agent"), "missing omp default path");
        assert!(
            script.contains("~/.omp/profiles"),
            "missing omp profile path"
        );
        assert!(
            script.contains("~/.local/share/omp"),
            "missing omp XDG path"
        );
        // Verify script structure
        assert!(script.contains("===PROBE_START==="));
        assert!(script.contains("===PROBE_END==="));
        assert!(script.contains("for dir in \"${PROBE_DIRS[@]}\""));
    }

    #[test]
    fn infer_agent_type_covers_all_dynamic_agents() {
        // Ensure infer_agent_type handles all agents from franken_agent_detection
        assert_eq!(infer_agent_type("~/.goose/sessions"), "goose");
        assert_eq!(infer_agent_type("~/.continue/sessions"), "continue");
        assert_eq!(infer_agent_type("~/.clawdbot/sessions"), "clawdbot");
        assert_eq!(infer_agent_type("~/.factory/sessions"), "factory");
        assert_eq!(infer_agent_type("~/.vibe/logs/session"), "vibe");
        assert_eq!(infer_agent_type("~/.windsurf"), "windsurf");
        assert_eq!(
            infer_agent_type("~/.config/Code/User/globalStorage/sourcegraph.amp"),
            "amp"
        );
        assert_eq!(infer_agent_type("~/.pi/agent/sessions"), "pi_agent");
        assert_eq!(infer_agent_type("~/.prime/agent/sessions"), "prime_agent");
        assert_eq!(infer_agent_type("/home/user/.prime/agent"), "prime_agent");
        assert_eq!(infer_agent_type("/home/user/.prime/agent-other"), "unknown");
        assert_eq!(infer_agent_type("~/.omp/agent/sessions"), "omp");
        assert_eq!(
            infer_agent_type("~/.omp/profiles/work/agent/sessions"),
            "omp"
        );
        assert_eq!(infer_agent_type("~/.local/share/omp/sessions"), "omp");
        assert_eq!(infer_agent_type("~/.local/share/omp"), "omp");
        assert_eq!(
            infer_agent_type("/srv/xdg/omp/profiles/work/sessions"),
            "omp"
        );
    }

    // =========================================================================
    // Deduplication tests
    // =========================================================================

    fn make_probe_result(
        name: &str,
        machine_id: Option<&str>,
        sessions: Option<u64>,
    ) -> HostProbeResult {
        HostProbeResult {
            host_name: name.to_string(),
            reachable: true,
            connection_time_ms: 100,
            cass_status: if let Some(s) = sessions {
                CassStatus::Indexed {
                    version: "0.1.50".into(),
                    session_count: s,
                    last_indexed: None,
                }
            } else {
                CassStatus::NotFound
            },
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                distro: Some("Ubuntu 25.10".into()),
                has_cargo: true,
                has_cargo_binstall: false,
                has_curl: true,
                has_wget: true,
                remote_home: "/home/ubuntu".into(),
                machine_id: machine_id.map(String::from),
            }),
            resources: Some(ResourceInfo {
                disk_available_mb: 800_000,
                memory_total_mb: 16_000,
                memory_available_mb: 8_000,
                can_compile: true,
            }),
            error: None,
        }
    }

    #[test]
    fn test_deduplicate_no_duplicates() {
        let results = vec![
            make_probe_result("host1", Some("machine-1"), Some(100)),
            make_probe_result("host2", Some("machine-2"), Some(200)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 2);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_deduplicate_same_machine() {
        // Two SSH aliases for the same machine
        let results = vec![
            make_probe_result("jain", Some("abc123"), None),
            make_probe_result("jain_ovh_box", Some("abc123"), None),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep "jain" (alphabetically first since neither has cass)
        assert_eq!(deduped[0].host_name, "jain");
        assert_eq!(
            merged.get("jain").unwrap(),
            &vec!["jain_ovh_box".to_string()]
        );
    }

    #[test]
    fn test_deduplicate_prefers_installed_cass() {
        // Two aliases, one with cass installed
        let results = vec![
            make_probe_result("alias_a", Some("machine-x"), None), // no cass
            make_probe_result("alias_b", Some("machine-x"), Some(500)), // has cass
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep alias_b because it has cass installed
        assert_eq!(deduped[0].host_name, "alias_b");
        assert!(merged.contains_key("alias_b"));
    }

    #[test]
    fn test_deduplicate_prefers_more_sessions() {
        // Both have cass, but different session counts
        let results = vec![
            make_probe_result("host_low", Some("machine-y"), Some(50)),
            make_probe_result("host_high", Some("machine-y"), Some(500)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep host_high because it has more sessions
        assert_eq!(deduped[0].host_name, "host_high");
        // Verify the merge recorded the merged alias
        assert!(merged.contains_key("host_high"));
    }

    #[test]
    fn test_deduplicate_no_machine_id_not_merged() {
        // Hosts without machine_id should not be merged
        let results = vec![
            make_probe_result("host1", None, Some(100)),
            make_probe_result("host2", None, Some(200)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 2);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_deduplicate_mixed_with_and_without_machine_id() {
        let results = vec![
            make_probe_result("aliasA", Some("same-machine"), Some(100)),
            make_probe_result("aliasB", Some("same-machine"), Some(50)),
            make_probe_result("standalone", None, Some(75)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        // 2 hosts: one from deduplication, one standalone
        assert_eq!(deduped.len(), 2);
        // aliasA should be kept (more sessions)
        assert!(deduped.iter().any(|h| h.host_name == "aliasA"));
        assert!(deduped.iter().any(|h| h.host_name == "standalone"));
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_deduplicate_three_aliases_same_machine() {
        let results = vec![
            make_probe_result("alias1", Some("same"), Some(100)),
            make_probe_result("alias2", Some("same"), Some(200)),
            make_probe_result("alias3", Some("same"), Some(150)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // alias2 has the most sessions
        assert_eq!(deduped[0].host_name, "alias2");
        // The merged list should contain the other two aliases
        let merged_list = merged.get("alias2").unwrap();
        assert_eq!(merged_list.len(), 2);
        assert!(merged_list.contains(&"alias1".to_string()));
        assert!(merged_list.contains(&"alias3".to_string()));
    }

    #[test]
    #[cfg(not(windows))]
    fn real_probe_machine_id_present() {
        // Test that the local probe script actually collects machine_id
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");

        // On Linux or macOS, we should get a machine_id
        // (this test may be skipped on unusual systems)
        if sys.os == "linux" || sys.os == "darwin" {
            assert!(
                sys.machine_id.is_some(),
                "machine_id should be present on {}",
                sys.os
            );
            let mid = sys.machine_id.as_ref().unwrap();
            assert!(!mid.is_empty(), "machine_id should not be empty");
            // Machine IDs are typically 32+ hex chars or UUID format
            assert!(
                mid.len() >= 32,
                "machine_id should be at least 32 chars, got: {}",
                mid
            );
        }
    }
}
