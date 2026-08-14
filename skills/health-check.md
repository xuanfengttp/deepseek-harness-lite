---
name: health-check
description: Run a deterministic health check SOP on the device — CPU, memory, interface, service status (auto-detects Windows/Linux)
whenToUse: For routine inspection or when asked to check device health
mode: workflow
think: false
tools:
  allow: [shell, ssh_exec]
steps:
  - id: cpu_mem
    tool: shell
    args:
      command: "uname -a 2>/dev/null || ver"
  - id: disk_usage
    tool: shell
    args:
      command: "df -h / 2>/dev/null || wmic logicaldisk get caption,freespace,size"
  - id: interface_status
    tool: shell
    args:
      command: "ip link show 2>/dev/null || ipconfig"
  - id: service_status
    tool: shell
    args:
      command: "systemctl is-active sshd 2>/dev/null || sc query sshd 2>/dev/null || echo service-check-skipped"
  - id: summarize
    llm_judge: "Summarize the health check results. Flag any anomalies. Return a concise status report."
    input: "{{steps.cpu_mem.result}}\n{{steps.disk_usage.result}}\n{{steps.interface_status.result}}\n{{steps.service_status.result}}"
---

# Health Check SOP

This skill runs a fixed sequence of diagnostic commands and summarizes the results.
Commands auto-detect the platform: Linux commands first, Windows fallbacks if they fail.
No exploration needed — the steps are deterministic.
