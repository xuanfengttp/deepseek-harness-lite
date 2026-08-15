---
name: health-check
description: Run a deterministic health check SOP on the local device — CPU, memory, interface, service status (auto-detects Windows/Linux)
whenToUse: For routine inspection of the local device or when asked to check device health
mode: workflow
think: false
tools:
  allow: [shell]
steps:
  - id: cpu_mem
    tool: shell
    args:
      command: "ver 2>nul || uname -a"
  - id: disk_usage
    tool: shell
    args:
      command: "dir C:\ 2>nul || df -h"
  - id: interface_status
    tool: shell
    args:
      command: "ipconfig 2>nul || ip addr"
  - id: service_status
    tool: shell
    args:
      command: "sc query sshd 2>nul || systemctl is-active sshd 2>nul || echo service-check-skipped"
  - id: summarize
    llm_judge: "Summarize the health check results. Flag any anomalies. Return a concise status report."
    input: "{{steps.cpu_mem.result}}\n{{steps.disk_usage.result}}\n{{steps.interface_status.result}}\n{{steps.service_status.result}}"
---

# Health Check SOP

This skill runs a fixed sequence of diagnostic commands on the **local device** and summarizes the results.
Commands try Windows first (ver/wmic/ipconfig/sc), fall back to Linux (uname/df/ip/systemctl).
No exploration needed — the steps are deterministic.

For **remote** device health checks via SSH, use the `remote-health-check` skill instead.
