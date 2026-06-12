---
name: feedback-wsl-commands
description: How to run shell commands for this project — must use wsl -d Ubuntu-24.04
metadata:
  type: feedback
---

Always run shell commands via `wsl -d Ubuntu-24.04 -- <command>`. Rust (rustc, cargo) is installed inside the Ubuntu-24.04 WSL distro, not on the Windows host. Running bare Bash commands or `bash -c` without the wsl prefix will fail with "command not found".

**Why:** User corrected this after bare bash/rustc lookup attempts failed.
**How to apply:** Every Bash tool call that needs the project environment should start with `wsl -d Ubuntu-24.04 --`.
