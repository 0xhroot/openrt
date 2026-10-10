//! `system.info` — host snapshot (OS/kernel, CPU, memory, disk, uptime).

use serde_json::{json, Value};

use openrt_resources::system::SystemInfo;

use crate::{Tool, ToolError, ToolOutput, ToolSpec};

pub struct SystemInfoTool {
    pub spec: ToolSpec,
}

impl SystemInfoTool {
    pub fn new() -> Self {
        let spec = ToolSpec::new(
            "system.info",
            "Reads a snapshot of host system information: OS, kernel, architecture, CPU count/model, memory, disk free space, uptime.",
        )
        .with_caps(&["system.info.all"])
        .with_schema(json!({ "type": "object" }));
        Self { spec }
    }
}

impl Default for SystemInfoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for SystemInfoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let info = SystemInfo::collect();
        let cpu_model = info
            .cpu
            .model
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        Ok(ToolOutput::json(json!({
            "os": info.os,
            "kernel": info.kernel,
            "arch": info.arch,
            "cpu_count": info.cpu.count,
            "cpu_model": cpu_model,
            "mem_total_mb": info.mem.total_kb / 1024,
            "mem_available_mb": info.mem.available_kb / 1024,
            "disk_avail_bytes": info.disk.avail_bytes,
            "uptime_secs": info.uptime_secs,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_json_snapshot() {
        let t = SystemInfoTool::new();
        let out = t.run(json!({})).await.unwrap();
        let v = out.try_typed::<Value>().unwrap();
        assert!(v.get("cpu_count").and_then(Value::as_u64).unwrap_or(0) > 0);
        assert!(v.get("mem_total_mb").is_some());
    }
}
