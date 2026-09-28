// Mapping fixture bytes: tests write them as LF or BOM+CRLF and never compile them.
pub fn sink() -> usize { 0 }
/* 한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백
한글 주석 ✓ 원문 byte 좌표 여백 */
pub fn inside() { sink(); }
pub fn decoy() { let _long = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; }
pub fn outside() { inside(); }
pub const LONE: usize = sink();
gen_impl!(Z);
pair!(unrelated, C);
enumd!(E);
two_fns!();
rev!();
foreign_impl!();
global_asm!("nop");
with_struct!();
only_types!();
