key: 690-input-single-path-herdr-busy-marker-window
| `input-single-path` Herdr busy 첫 시도와 📬 (#8113 수리 1) | `2f32ed6e23` | Before: prompt 뒤 session marker 구간의 busy 판독이 Unknown이라 첫 시도 Held→📬, Herdr 커밋도 📬 선게시 | Fixed: marker 뒤 prompt/tool_result를 실행 중으로 읽고 커밋 📬를 owner 결과로 미룸. 8 cfg GREEN3/expected RED5 일치. [보고서](input-single-path/herdr-busy-marker-window.report.md). |
