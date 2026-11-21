# SOW_20261121_BWUUA0P7 — Tích hợp upstream vào codex/combined

- Status: DONE
- Approval: User duyệt tích hợp upstream và test cô lập ngày 2026-11-21. Release baseline Terra cần báo trước. Push 4 commit hiện có thuộc approval riêng.
- Proposed-By: Codex
- create_dttm: 2026-11-21T21:39:48+07:00
- approve_dttm: 2026-11-21T22:45:41+07:00
- finish_dttm: 2026-11-22T02:15:29+07:00
- Reference: K4R7M2Q9 — Shared Codex fork, 6ZBV6X33 — Context budget, RTC0QW6N — Linux proc fallback, QY40PLXI/A4CWARH3 — Native Takeover HOLD, F3TAIJSK/T2 — Terra-6.1.

## Task / Why

Tích hợp upstream mới vào checkout hiện có, giữ hợp đồng XVision/router còn dùng và thu gọn patch đã có upstream tương đương. Worker 0.0.0 đang thiếu Terra-6.1 trong catalog xác thực, nên kiểm riêng provenance và version của artifact thử.

```text
As-is: upstream aa854dab → 16 commit downstream → fork c1ec7c5
To-be: upstream 1145028a ─┐
       fork c1ec7c5 ─────┴→ merge local → xác minh patch → revision ứng viên
                                                     A4CWARH3 lưu trong history
```

## Location / Deliverables

- Owner: `tools/codex`, nhánh hiện có `codex/combined`. Pin snapshot `upstream/main@1145028ae47e8119955323d38ce34122d418f951` quan sát ngày 2026-11-21. Khi triển khai, fetch đúng pin, lấy đủ lịch sử và ghi merge base đã xác minh qua shallow boundary trước khi merge. [Upstream commit](https://github.com/openai/codex/commit/1145028ae47e8119955323d38ce34122d418f951)
- Sửa tích hợp có chủ đích trong `codex-rs/app-server{,-protocol}/`, `codex-rs/core/`, `codex-rs/{protocol,history,state,thread-store,linux-sandbox,hooks,config,analytics,exec,tui}/` và SDK/generated files đã được downstream chạm. Đường dẫn ngoài danh sách chỉ được nhập nguyên từ merge upstream.
- So status/diff và hash từng file untracked trước/sau, gồm toàn bộ file trong `docs/proposals/` và `plan_todo/`. Giữ phần đang dở trong `docs/downstream-patches.md` và các file ghi nhận được, ngoại trừ cập nhật evidence có chủ đích vào SOW. Dừng nếu merge đụng phần dirty; chốt cách xử lý trước khi tiếp tục.
- Deliverables: revision tích hợp local, bảng verdict từng nhóm patch, kết quả test và manifest artifact thử. Bảo toàn lịch sử fork. Trước build, bắt buộc loại A4CWARH3 khỏi candidate bằng thay đổi riêng, có thể dùng revert có review. Lưu `c1ec7c5` trong history và giữ phần upstream mới khi giải conflict.

## Verdict patch tại upstream 1145028a

| Patch | Verdict | Evidence |
| --- | --- | --- |
| Idle dynamic tools · K4R7M2Q9 | **Giữ** | Upstream JSON schema chưa có `thread/resume.dynamicTools` và `dynamicToolsDigest`. RPC PASS preserve/clear/replace, active refusal và cold resume. Fork/revert PASS theo boundary. [Params](https://github.com/openai/codex/blob/1145028ae47e8119955323d38ce34122d418f951/codex-rs/app-server-protocol/schema/json/v2/ThreadResumeParams.json) · [Response](https://github.com/openai/codex/blob/1145028ae47e8119955323d38ce34122d418f951/codex-rs/app-server-protocol/schema/json/v2/ThreadResumeResponse.json) |
| Reload custom model catalog · K4R7M2Q9 | **Giữ** | Upstream `current()` reload khi bị invalidate. Chưa thấy đường tự phát hiện file đổi trong hàm này. Unit và `model/list` PASS cùng process, kể cả file mới cùng mtime và file lỗi giữ snapshot cũ. [Upstream catalog](https://github.com/openai/codex/blob/1145028ae47e8119955323d38ce34122d418f951/codex-rs/app-server/src/model_catalog.rs) |
| Context budget ACK · 6ZBV6X33 | **Giữ** | Upstream start/resume schema chưa có `contextBudget`. XVision đang dùng ACK để đối soát window/auto-compact. Start/resume và Compact PASS. [Start response](https://github.com/openai/codex/blob/1145028ae47e8119955323d38ce34122d418f951/codex-rs/app-server-protocol/schema/json/v2/ThreadStartResponse.json) |
| Alpine proc fallback · RTC0QW6N | **Dùng implementation upstream** | Classifier upstream nhận cả `/newroot/proc` và `/proc`, gồm permission-denied và invalid-argument. Diff production classifier bằng upstream. Alpine 4/4 PASS: hai dạng proc denial, proxy fail-closed và direct egress bị chặn. [Upstream classifier](https://github.com/openai/codex/blob/1145028ae47e8119955323d38ce34122d418f951/codex-rs/linux-sandbox/src/linux_run_main.rs#L1265) |
| Vendor provenance · 7BAJO66L | **Lưu trữ** | Giữ `6d5a488` trong history. Patch và lockfile archive thuộc provenance. |
| Native Takeover · A4CWARH3 | **HOLD** | Giữ prototype ở commit gốc. Tree ứng viên và artifact build thử phải loại phần HOLD, có negative check. |

Trace commit: dynamic tools `d551698 717b0f8 7673f9f 1fb4a7f cb81389 1addaf6`; catalog `a1d9c07 77cebe4 2a8025b be2f450`; tài liệu chung `5ac89bf 8ea82f5`; context ACK `6c32c88`; Linux proc `751a134`; archive `6d5a488`; HOLD `c1ec7c5`.

## Done Criteria

1. Local integration có đúng upstream trong ancestry và giữ lịch sử fork. Mỗi patch có verdict kèm diff hoặc test. Upstream tương đương phải giữ behavior, tránh revert cả thay đổi upstream. Giữ nguyên phần dirty của người dùng.
2. Rust tests: dynamic tools idle/cold resume và từ chối active mutation, generated protocol, context ACK, Linux proc classifier/managed-proxy. Với catalog, dùng test `model_catalog` và RPC `model_list_requirements_tests`: cùng process thấy file replace dù mtime giữ nguyên; file lỗi vẫn giữ catalog hợp lệ trước đó.
3. Build thử `dev-small` từ đúng candidate: macOS `codex-app-server`, macOS `codex-code-mode-host` và Linux guest executor. Ghi Git SHA, target, đường dẫn, version và SHA-256 từng binary; đối chiếu với executable thực sự chạy. Dùng app-server thử với CODEX_HOME tạm và guest disposable theo sandbox hiện có. Kiểm `environment/add → status ready → info`, một native tool/code-mode qua guest có receipt, và artifact từ chối capability HOLD. Dọn process/guest thử. Thiếu cách chạy cô lập thì giữ DC này mở.
4. Pin main mang placeholder `0.0.0`. Tag `rust-v0.171.0` (`ccf9be8c`) đã phân nhánh so với pin. Build main vẫn có thể nghiệm thu tích hợp trong môi trường thử. Đánh giá catalog Terra cần chốt release baseline và kiểm `client_version` thực gửi từ artifact tương ứng. F3TAIJSK/T2 vẫn cần lượt Terra-6.1 thật qua UI4187 sau khi rollout được duyệt riêng. [So sánh tag và main](https://github.com/openai/codex/compare/ccf9be8cc458bd6b8336fe84d68d8893f2691a5e...1145028ae47e8119955323d38ce34122d418f951)

## Out-of-scope / Rủi ro

- Rollout, thay binary, restart XVision/Desktop, chỉnh auth/catalog đang chạy, nới sandbox, sửa codex-router, thao tác Bronze, push revision tích hợp hoặc force-push.
- Catalog có model chỉ chứng minh discovery. Guest protocol và inference cần evidence riêng. Merge conflict ngoài Location phải được đưa lại vào scope review.

## Progress

- progress_updated_dttm: 2026-11-22T01:04:46+07:00
- [x] P0: 4 commit đã push fast-forward lên fork, remote readback `c1ec7c5`.
- [x] P1: kiểm sơ bộ source/schema upstream, ghi bảng giữ patch.
- [x] R1: Luna-6 Max rà pin/version, 16 commit, HOLD và dirty work. Đã cập nhật scope và DC.
- [x] R2: Luna-6 Max rà draft sau R1. Đã chốt test cô lập, manifest từng binary và catalog pass/fail.
- [x] P2: merge local `270d308a` có parents `35e012dd` + `1145028a`, base `aa854dab`. HOLD `c1ec7c5` trong history và bị loại khỏi candidate. Stable/experimental exports và Python SDK đã regenerate. Dirty docs và fork `c1ec7c5` còn nguyên.
- [x] P3: 33 Cargo tests PASS (schema 4, history 2, thread store 11, catalog 2, dynamic/context RPC 9, Compact 1, Alpine 4) và 2 test classifier Rust độc lập. Ba binary cùng cây nguồn build PASS. Probe dùng sandbox Darwin/Alpine XVision và model fixture loopback: environment add/ready/info, code-mode → exec_command có receipt Linux UID/workspace, không có capability A4CWARH3. Hai guest đã dọn.
- [x] P4: [Release baseline 0.171.0](EXT_release_0_171_0.md) đã build, probe và rollout XVision. Inference Terra-6.1 thật đạt tại XVision F3TAIJSK/T2.

### Đối soát closeout

- DC1: upstream `1145028a` và fork `c1ec7c5` đều nằm trong ancestry `12177bd41`; verdict patch và HOLD ghi ở trên.
- DC2: 33 Cargo checks, 2 classifier checks và 4 ca Alpine managed-proxy PASS theo evidence P3.
- DC3: ba binary cùng revision `12177bd41`; manifest SHA và guest receipt tại XVision `.runtime/codex-rollouts/12177bd4/`. Guest code-mode và negative HOLD PASS.
- DC4: UI4187 dùng OpenAI account hiện có, native turn `01a1ff55-b2be-74f0-ab3c-49f4dc3d9477` xác nhận `gpt-6.1-terra`, effort `high`, hoàn tất và trả `42`. Chi tiết tại XVision `plan_todo/finished/SOW_20261119_F3TAIJSK/`.
- Fork remote giữ `270d308a`; release commit `12177bd41` hiện chỉ ở local, chưa push theo yêu cầu.

## Evidence

- macOS: `cargo test --locked --profile dev-small` theo các nhóm P3.
- Linux: cross-build test `codex-linux-sandbox --test all`, chạy 4 ca chọn lọc trong Alpine riêng, 0 skip.
- Logs: `targeted-tests-verified-v8.log`, `guest-probe-final.log`, `managed-probe.log` trong scratch.

## Artifact thử

Source `270d308a76b4ee5f2681a901203e873b9ee791a2`, tree `5d48ac317619ddb4c63beb10529b642a4a164211`. Cargo `dev-small --locked`, Rust 1.99.1, V8 152.2.0 theo SHA-256 trong `MODULE.bazel`.

Scratch `/var/folders/j7/9l12vn_x0x75vtzk0ys6vzdc0000gn/T/bwuua0p7.vATj6wn6lo`.

| Binary | Target | Version | SHA-256 |
| --- | --- | --- | --- |
| `mac-artifacts/codex-app-server` | aarch64-apple-darwin | 0.0.0 trực tiếp | `e4fc7c1ffceddb93ac11c9748642f5b00481cfa5e1224f04db70ce706b19d161` |
| `mac-artifacts/codex-code-mode-host` | aarch64-apple-darwin | 0.0.0 package | `f47783b2fc0c385ec586c0a78813f3b62d0cdb805f7c81fa2af1636cba1dd893` |
| `linux-artifacts/codex` | aarch64-unknown-linux-musl | 0.0.0 trong guest | `efb3f8733cfb07188cee2efe90a327c0a8c9454f59e510b5cfda2be7b0ab4f51` |

Build Linux: Zig 0.17 không hỗ trợ `--fix-cortex-a53-843419` từ Rust 1.99.1. Dùng LLVM LLD cho link executable trong scratch và giữ nguyên cờ. V8 dùng archive/binding OpenAI đã xác minh theo `MODULE.bazel`.

Fork readback `c1ec7c5`. Hash hai binary XVision khớp trước/sau, UI4187 HTTP 200. Build/cache riêng đã dọn, giữ 3 binary và logs trong scratch.

Điều phối. Native GLM-5.3 Max trả 403 trước khi chạy slice rà test; coordinator tiếp tục local, không đổi credential.

## Rollout XVision dev

- 2026-11-22T00:05:43+07:00. User duyệt push và rollout dev. Fork `codex/combined` đọc lại đúng `270d308a`.
- Harness UI4187 chỉ dừng khi Agent/refresh idle. Cài ba artifact cùng commit; giữ bundle cũ trong `.runtime/codex-rollouts/270d308a/rollback`.
- UI HTTP 200, API startup đạt, PID app-server khớp inode artifact mới. Guest thật chạy executor mới và SHA-256 khớp manifest. App-server và guest cùng báo `0.0.0` theo version trong Cargo.
- Rollout này xác nhận khởi động và artifact 0.0.0. Rollout 0.171.0 và pin helper mới nằm trong [EXT release](EXT_release_0_171_0.md).
