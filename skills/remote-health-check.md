---
name: remote-health-check
description: SSH into a remote network device and run a deterministic health check SOP — version, interfaces, counters, CPU/memory (show commands)
whenToUse: For routine inspection of a remote network device via SSH, or when asked to check a remote device's health
mode: workflow
think: false
tools:
  allow: [ssh_exec]
variables:
  target: "core-router"
steps:
  - id: show_version
    tool: ssh_exec
    args:
      command: "show version"
      target: "{{target}}"
  - id: show_interface
    tool: ssh_exec
    args:
      command: "show ip interface brief"
      target: "{{target}}"
  - id: show_cpu_mem
    tool: ssh_exec
    args:
      command: "show processes cpu sorted | head 20"
      target: "{{target}}"
  - id: show_counters
    tool: ssh_exec
    args:
      command: "show interfaces stats"
      target: "{{target}}"
  - id: show_env
    tool: ssh_exec
    args:
      command: "show environment all 2>nul || show env all 2>nul || echo env-check-skipped"
      target: "{{target}}"
  - id: analyze
    llm_judge: |
      分析以下网络设备 SSH 巡检输出，生成健康报告：

      1. 设备型号和版本（show version）
      2. 接口状态摘要：up/down 数量，列出异常接口
      3. CPU/内存使用情况，标注高负载
      4. 接口计数器异常（错误、丢包、CRC）
      5. 环境/电源状态（如可用）
      6. 总体健康评级：✅ 正常 / ⚠️ 关注 / ❌ 异常
      7. 建议（如有异常）

      用简洁的表格或列表格式输出。
    input: |
      ===== show version =====
      {{steps.show_version.result}}

      ===== show ip interface brief =====
      {{steps.show_interface.result}}

      ===== show processes cpu =====
      {{steps.show_cpu_mem.result}}

      ===== show interfaces stats =====
      {{steps.show_counters.result}}

      ===== show environment =====
      {{steps.show_env.result}}
---

# Remote Health Check SOP

This skill SSHes into a remote network device and runs a fixed sequence of
`show` commands, then summarizes the results into a health report.

## Usage

Switch the `target` variable to the device name configured in `ssh.targets`:

```
variables:
  target: "edge-switch"
```

Or override at invocation time by editing the skill variables before running.

## What it checks

| Step | Command | Purpose |
|------|---------|---------|
| show_version | `show version` | Device model, OS version, uptime |
| show_interface | `show ip interface brief` | Interface up/down status |
| show_cpu_mem | `show processes cpu sorted \| head 20` | CPU load, top processes |
| show_counters | `show interfaces stats` | Interface error/drop counters |
| show_env | `show environment all` | Power, fan, temperature (if supported) |

## Notes

- SSH session persists across all steps — no reconnection overhead
- Commands use Cisco IOS syntax. For Huawei/Juniper, adjust the command strings
- `show environment` may not be supported on all platforms (gracefully skipped)
- This is a **workflow** skill (deterministic, no LLM exploration) — fast and
  reliable on small models
