# EXT — Baseline Codex 0.171.0 cho XVision

- Status: DONE
- Approval: User duyệt baseline `rust-v0.171.0`, build và probe cô lập, rollout khi đạt.
- create_dttm: 2026-11-22T00:31:34+07:00
- approve_dttm: 2026-11-22T00:31:34+07:00
- finish_dttm: 2026-11-22T01:04:46+07:00
- Owner: checkout `tools/codex`, nhánh `codex/combined`.
- Reference: BWUUA0P7/P4, XVision F3TAIJSK/T2.

## Scope

- As-is: fork `270d308a` báo `0.0.0`; cùng account chỉ nhận 6 model.
- To-be: fork hiện tại + version release `ccf9be8c` → 3 binary cùng commit → probe → rollout.
- Location: `codex-rs/Cargo.toml`, `codex-rs/Cargo.lock`, EXT này.
- Decision: lấy commit khai báo version từ tag. Giữ cây main đã nghiệm thu và toàn bộ patch XVision. Tag có thêm bản backport TUI status line, tính năng đã có trên main qua `2d5816565`.
- Boundaries: A4CWARH3 HOLD; sandbox, stock Desktop, Bronze và router source giữ nguyên. Push cần duyệt riêng. XVision giữ helper rebuild khớp SHA mới.
- Risk: version đúng vẫn cần catalog live, guest protocol và rollback qua bộ binary cũ.

## Checklist

- [x] R1 `12177bd41` chỉ đổi workspace version và 179 version nội bộ trong lockfile. Graph, checksum dependency và cây implementation khớp `270d308a`.
- [x] R2 Build ba binary với Rust 1.99.1, `--locked --profile dev-small`. Guest Alpine riêng PASS code-mode → exec, Linux UID/workspace/marker, cleanup và negative HOLD.
- [x] R3 Restart UI4187 lúc idle. App-server và executor live báo 0.171.0; UI OpenAI và catalog mới xác nhận đủ bốn ID. Pin helper đồng bộ.

- Cùng account: `0.0.0` nhận 6 model, thiếu 4 ID. `0.171.0` nhận 12, có `gpt-6-luna`, `gpt-6.1-terra`, `gpt-6.2-astra`, `gpt-6.2-sol`.
- 2026-11-22 01:04 ICT. Receipt và rollback ở XVision `.runtime/codex-rollouts/12177bd4/`. F3TAIJSK/T2 đã xác nhận inference Terra thật, xem SOW cha.
