---
trigger: always_on
---

To search code, use the `rg` command (i.e. ripgrep) instead of `grep`. The `rg` command 
automatically ignores files in the `.gitignore` list, which avoids searching binary 
files and other non-editable files.

To run the tests with all optional Cargo features enabled, use the `cargo test-all` command.