//! `malloc` & co. de tout le processus sur le tas de RadixOx (MEMORY_ARCH.md §18.13) :
//! la libc elle-même (binaire `std`) et le C lié (zstd) n'allouent plus chez la libc.
//! Inclus par les deux binaires (`#[path]`), pas sous `radixox_sysalloc`.

use radixox_lib::raw_alloc::heap::c;
use core::ffi::c_void;

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: usize) -> *mut c_void {
    c::malloc(size) as _
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(p: *mut c_void) {
    unsafe { c::free(p as _) }
}
#[unsafe(no_mangle)]
pub extern "C" fn calloc(n: usize, size: usize) -> *mut c_void {
    c::calloc(n, size) as _
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
    unsafe { c::realloc(p as _, size) as _ }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reallocarray(p: *mut c_void, n: usize, size: usize) -> *mut c_void {
    unsafe { c::reallocarray(p as _, n, size) as _ }
}
#[unsafe(no_mangle)]
pub extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
    c::aligned_alloc(align, size) as _
}
#[unsafe(no_mangle)]
pub extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
    c::aligned_alloc(align, size) as _
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32 {
    unsafe { c::posix_memalign(out as _, align, size) }
}
#[unsafe(no_mangle)]
pub extern "C" fn valloc(size: usize) -> *mut c_void {
    c::valloc(size) as _
}
#[unsafe(no_mangle)]
pub extern "C" fn pvalloc(size: usize) -> *mut c_void {
    c::pvalloc(size) as _
}
#[unsafe(no_mangle)]
pub extern "C" fn malloc_usable_size(p: *mut c_void) -> usize {
    c::usable_size(p as _)
}
