//! CPU and memory monitoring for managed service processes.
//!
//! [`ResourceMonitor`] wraps `sysinfo` and samples each tracked process on a
//! periodic tick, writing the latest `cpu_percent` and `memory_bytes` back into
//! the shared [`SharedProcess`] entries so the dashboard can display live stats.
//!
//! # Author
//! Daniel Tamas <hello@danieltamas.com>

use sysinfo::{Pid, System};

use crate::core::process::SharedProcess;

pub struct ResourceMonitor {
    system: System,
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self {
            system: System::new_all(),
        }
    }

    pub async fn update(&mut self, processes: &[SharedProcess]) {
        self.system.refresh_all();

        for proc in processes {
            let pid = {
                let p = proc.lock().await;
                p.pid
            };

            if let Some(pid) = pid {
                let (cpu, mem) = self.get_stats(pid);
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

    fn get_stats(&self, pid: u32) -> (f32, u64) {
        let sysinfo_pid = Pid::from(pid as usize);
        if let Some(process) = self.system.process(sysinfo_pid) {
            let cpu = process.cpu_usage();
            let mem = process.memory(); // bytes
            (cpu, mem)
        } else {
            (0.0, 0)
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
