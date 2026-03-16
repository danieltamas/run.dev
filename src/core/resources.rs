//! CPU and memory monitoring for managed service processes.
//!
//! [`ResourceMonitor`] wraps `sysinfo` and samples each tracked process on a
//! periodic tick, writing the latest `cpu_percent` and `memory_bytes` back into
//! the shared [`SharedProcess`] entries so the dashboard can display live stats.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::core::process::SharedProcess;

pub struct ResourceMonitor {
    system: System,
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    pub async fn update(&mut self, processes: &[SharedProcess]) {
        // Collect PIDs first, then refresh only those processes — not the entire system
        let mut pids: Vec<(usize, Pid)> = Vec::new();
        for (i, proc) in processes.iter().enumerate() {
            let p = proc.lock().await;
            if let Some(pid) = p.pid {
                pids.push((i, Pid::from(pid as usize)));
            }
        }

        if !pids.is_empty() {
            let pid_refs: Vec<Pid> = pids.iter().map(|(_, pid)| *pid).collect();
            self.system.refresh_processes(ProcessesToUpdate::Some(&pid_refs));
        }

        for (i, proc) in processes.iter().enumerate() {
            if let Some((_, sysinfo_pid)) = pids.iter().find(|(idx, _)| *idx == i) {
                let (cpu, mem) = if let Some(process) = self.system.process(*sysinfo_pid) {
                    (process.cpu_usage(), process.memory())
                } else {
                    (0.0, 0)
                };
                let mut p = proc.lock().await;
                p.cpu_percent = cpu;
                p.memory_bytes = mem;
            } else {
                let mut p = proc.lock().await;
                p.cpu_percent = 0.0;
                p.memory_bytes = 0;
            }
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{}M", bytes / (1024 * 1024))
    } else {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn format_cpu(cpu: f32) -> String {
    format!("{:.1}%", cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_kilobytes() {
        assert_eq!(format_bytes(512 * 1024), "512K");
    }

    #[test]
    fn format_bytes_megabytes() {
        assert_eq!(format_bytes(128 * 1024 * 1024), "128M");
    }

    #[test]
    fn format_bytes_gigabytes() {
        let bytes = (1.5 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert_eq!(format_bytes(bytes), "1.5G");
    }

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0K");
    }

    #[test]
    fn format_bytes_boundary_1mb() {
        // exactly 1MB should use MB format
        assert_eq!(format_bytes(1024 * 1024), "1M");
    }

    #[test]
    fn format_cpu_zero() {
        assert_eq!(format_cpu(0.0), "0.0%");
    }

    #[test]
    fn format_cpu_whole() {
        assert_eq!(format_cpu(25.0), "25.0%");
    }

    #[test]
    fn format_cpu_decimal() {
        assert_eq!(format_cpu(3.75), "3.8%");
    }

    #[test]
    fn format_cpu_hundred() {
        assert_eq!(format_cpu(100.0), "100.0%");
    }
}
