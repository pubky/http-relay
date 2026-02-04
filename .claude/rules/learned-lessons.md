# Lessons Learned

- Group related functionality in one folder (e.g., handlers with their server)
- Tests belong in the same file as the code they test (Rust `#[cfg(test)]` pattern)
- mod.rs only contains imports/exports, not actual logic
- CLI flag naming: use consistent prefixes for related params (e.g., `--link2-timeout`, `--link2-cache-ttl`)
- Ask about security model before suggesting fixes - apparent "issues" may be intentional design (e.g., ID as shared secret, open CORS for public relays)
- Consider O(n) vs O(1) when choosing data structures - present tradeoffs and let user decide (e.g., LRU vs HashMap for bounded caches)
- README is for onboarding devs - security/implementation docs belong in code docstrings near relevant code
- Comments: explain WHY, not WHAT - don't add comments that repeat what code already says (e.g., `// Check if X enabled` before `if config.x_enabled`)
- Move fallible operations outside `tokio::spawn` so errors propagate via `?` instead of panicking silently in background tasks
- Check actual default values from code (e.g., CLI args) instead of assuming common defaults like 3000
- TCP can't detect sudden disconnects (Wi-Fi off, app killed) - server writes succeed locally while packets vanish; caching is the safety net, not app-level ACK
- Oneshot channel sends: never ignore `let _ = sender.send(...)` - check result and handle stale receivers (e.g., disconnected clients leave orphan entries in waiting lists)
- API design for mobile: prefer long-poll endpoints over polling loops - simpler client code, fewer requests, works with OS backgrounding
