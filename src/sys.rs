//! Прямые системные вызовы Linux x86_64. Без libc и без std.
//! Здесь же: точка входа _start, аллокатор поверх mmap, mem*-функции, panic-обработчик.

use alloc::{string::String, vec::Vec};
use core::alloc::{GlobalAlloc, Layout};
use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("odjkbar: без libc поддержан только Linux x86_64");

// ---------- сырой syscall ----------

#[inline(always)]
unsafe fn sc(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    asm!(
        "syscall",
        inlateout("rax") n as isize => r,
        in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
        lateout("rcx") _, lateout("r11") _,
        options(nostack),
    );
    r
}

/// Ошибка ядра приходит как -errno в диапазоне -4095..-1.
fn res(r: isize) -> Result<usize, i32> {
    if (-4095..0).contains(&r) { Err((-r) as i32) } else { Ok(r as usize) }
}

const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_OPEN: usize = 2;
const SYS_CLOSE: usize = 3;
const SYS_PREAD64: usize = 17;
const SYS_POLL: usize = 7;
const SYS_MMAP: usize = 9;
const SYS_MADVISE: usize = 28;
const SYS_MUNMAP: usize = 11;
const SYS_RT_SIGACTION: usize = 13;
const SYS_MREMAP: usize = 25;
const SYS_DUP2: usize = 33;
const SYS_SOCKET: usize = 41;
const SYS_CONNECT: usize = 42;
const SYS_FORK: usize = 57;
const SYS_EXECVE: usize = 59;
const SYS_GETEUID: usize = 107;
const SYS_CLOCK_GETTIME: usize = 228;
const SYS_EXIT_GROUP: usize = 231;
const SYS_PIPE2: usize = 293;

const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CLOEXEC: usize = 0o2000000;

pub fn exit(code: i32) -> ! {
    loop {
        unsafe { sc(SYS_EXIT_GROUP, code as usize, 0, 0, 0, 0, 0) };
    }
}

// ---------- ввод-вывод ----------

/// Один read; EINTR повторяется. Ok(0) -- конец потока.
pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    loop {
        match res(unsafe { sc(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0) }) {
            Err(EINTR) => continue,
            r => return r,
        }
    }
}

/// Читает ровно buf.len() байт.
pub fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        match read(fd, &mut buf[got..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => got += n,
        }
    }
    true
}

pub fn write_all(fd: i32, mut data: &[u8]) -> bool {
    while !data.is_empty() {
        match res(unsafe { sc(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len(), 0, 0, 0) }) {
            Ok(0) => return false,
            Ok(n) => data = &data[n..],
            Err(EINTR) => {}
            Err(EAGAIN) => {
                let mut p = [PollFd { fd, events: POLLOUT, revents: 0 }];
                poll(&mut p, -1);
            }
            Err(_) => return false,
        }
    }
    true
}

pub fn close(fd: i32) {
    unsafe { sc(SYS_CLOSE, fd as usize, 0, 0, 0, 0, 0) };
}

fn cstr(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s);
    v.push(0);
    v
}

fn open(path: &str, flags: usize) -> Option<i32> {
    let p = cstr(path.as_bytes());
    res(unsafe { sc(SYS_OPEN, p.as_ptr() as usize, flags | O_CLOEXEC, 0, 0, 0, 0) }).ok().map(|f| f as i32)
}

/// Файл целиком.
pub fn read_file(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, O_RDONLY)?;
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    let ok = loop {
        match read(fd, &mut tmp) {
            Ok(0) => break true,
            Ok(n) => out.extend_from_slice(&tmp[..n]),
            Err(_) => break false,
        }
    };
    close(fd);
    if ok { Some(out) } else { None }
}

/// Файл, отображённый в память (только чтение). Отображение живёт до конца процесса.
/// Открыть файл только для чтения (шрифты держим открытыми, но не отображаем).
pub fn open_ro(path: &str) -> Option<i32> {
    open(path, O_RDONLY)
}

/// Чтение с абсолютного смещения (pread64). false -- ошибка или меньше байт, чем нужно.
pub fn pread(fd: i32, off: u64, buf: &mut [u8]) -> bool {
    let mut done = 0;
    while done < buf.len() {
        let r = res(unsafe {
            sc(SYS_PREAD64, fd as usize, buf[done..].as_mut_ptr() as usize, buf.len() - done, off as usize + done, 0, 0)
        });
        match r {
            Ok(0) | Err(_) => return false,
            Ok(n) => done += n,
        }
    }
    true
}

pub fn geteuid() -> u32 {
    unsafe { sc(SYS_GETEUID, 0, 0, 0, 0, 0, 0) as u32 }
}

/// Unix-время в миллисекундах (CLOCK_REALTIME).
pub fn now_ms() -> u64 {
    let mut ts = [0i64; 2];
    unsafe { sc(SYS_CLOCK_GETTIME, 0, ts.as_mut_ptr() as usize, 0, 0, 0, 0) };
    (ts[0] as u64) * 1000 + (ts[1] as u64) / 1_000_000
}

// ---------- poll ----------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub const POLLIN: i16 = 0x1;
pub const POLLOUT: i16 = 0x4;
pub const POLLERR: i16 = 0x8;
pub const POLLHUP: i16 = 0x10;

/// Отрицательные fd игнорируются ядром. timeout_ms < 0 -- ждать без ограничения.
pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> isize {
    unsafe { sc(SYS_POLL, fds.as_mut_ptr() as usize, fds.len(), timeout_ms as usize, 0, 0, 0) }
}

// ---------- сокеты ----------

/// Подключение к unix-сокету. abstract_ns -- имя в абстрактном пространстве (без ведущего \0).
pub fn connect_unix(path: &str, abstract_ns: bool) -> Option<i32> {
    let name = path.as_bytes();
    if name.len() > 107 {
        return None;
    }
    // AF_UNIX = 1, SOCK_STREAM = 1
    let fd = res(unsafe { sc(SYS_SOCKET, 1, 1 | O_CLOEXEC, 0, 0, 0, 0) }).ok()? as i32;
    let mut addr = [0u8; 110];
    addr[0] = 1;
    let (off, len) = if abstract_ns { (3, 3 + name.len()) } else { (2, 2 + name.len() + 1) };
    addr[off..off + name.len()].copy_from_slice(name);
    let r = res(unsafe { sc(SYS_CONNECT, fd as usize, addr.as_ptr() as usize, len, 0, 0, 0) });
    if r.is_err() {
        close(fd);
        return None;
    }
    Some(fd)
}

// ---------- процессы ----------

static mut ENVP: *const *const u8 = core::ptr::null();

pub fn getenv(name: &str) -> Option<&'static str> {
    unsafe {
        let mut p = ENVP;
        if p.is_null() {
            return None;
        }
        while !(*p).is_null() {
            let mut n = 0;
            while *(*p).add(n) != 0 {
                n += 1;
            }
            let e = core::slice::from_raw_parts(*p, n);
            if e.len() > name.len() && &e[..name.len()] == name.as_bytes() && e[name.len()] == b'=' {
                return core::str::from_utf8(&e[name.len() + 1..]).ok();
            }
            p = p.add(1);
        }
    }
    None
}

fn sigaction(sig: usize, handler: usize) {
    // struct kernel_sigaction: handler, flags, restorer, mask
    let act: [usize; 4] = [handler, 0, 0, 0];
    unsafe { sc(SYS_RT_SIGACTION, sig, act.as_ptr() as usize, 0, 8, 0, 0) };
}

/// SIGCHLD игнорируется (ядро само убирает зомби), SIGPIPE -- чтобы запись в закрытый сокет не убивала бар.
pub fn init_signals() {
    sigaction(17, 1);
    sigaction(13, 1);
}

/// Запускает программу (поиск по $PATH, как execvp). stdin и stderr -- /dev/null.
/// capture: stdout в канал, возвращается его читающий конец; иначе stdout тоже /dev/null.
pub fn spawn(argv: &[&str], capture: bool) -> Option<i32> {
    let prog = *argv.first()?;
    // всё, что требует выделения памяти, готовим до fork
    let args: Vec<Vec<u8>> = argv.iter().map(|a| cstr(a.as_bytes())).collect();
    let mut ptrs: Vec<usize> = args.iter().map(|a| a.as_ptr() as usize).collect();
    ptrs.push(0);
    let mut cands: Vec<Vec<u8>> = Vec::new();
    if prog.contains('/') {
        cands.push(cstr(prog.as_bytes()));
    } else {
        let path = getenv("PATH").unwrap_or("/usr/local/bin:/usr/bin:/bin");
        for dir in path.split(':') {
            let mut p: Vec<u8> = Vec::new();
            p.extend_from_slice(if dir.is_empty() { b"." } else { dir.as_bytes() });
            p.push(b'/');
            p.extend_from_slice(prog.as_bytes());
            p.push(0);
            cands.push(p);
        }
    }
    let devnull = cstr(b"/dev/null");
    let mut pipe = [-1i32; 2];
    if capture {
        res(unsafe { sc(SYS_PIPE2, pipe.as_mut_ptr() as usize, O_CLOEXEC, 0, 0, 0, 0) }).ok()?;
    }
    let pid = res(unsafe { sc(SYS_FORK, 0, 0, 0, 0, 0, 0) });
    match pid {
        Err(_) => {
            if capture {
                close(pipe[0]);
                close(pipe[1]);
            }
            None
        }
        Ok(0) => unsafe {
            // потомок: вернуть сигналам стандартное поведение, перенаправить потоки, exec
            sigaction(17, 0);
            sigaction(13, 0);
            let nul = sc(SYS_OPEN, devnull.as_ptr() as usize, O_RDWR | O_CLOEXEC, 0, 0, 0, 0);
            if nul >= 0 {
                sc(SYS_DUP2, nul as usize, 0, 0, 0, 0, 0);
                sc(SYS_DUP2, if capture { pipe[1] as usize } else { nul as usize }, 1, 0, 0, 0, 0);
                sc(SYS_DUP2, nul as usize, 2, 0, 0, 0, 0);
            }
            for c in &cands {
                sc(SYS_EXECVE, c.as_ptr() as usize, ptrs.as_ptr() as usize, ENVP as usize, 0, 0, 0);
            }
            exit(127)
        },
        Ok(_) => {
            if capture {
                close(pipe[1]);
                Some(pipe[0])
            } else {
                Some(-1)
            }
        }
    }
}

// ---------- точка входа ----------

global_asm!(
    ".globl _start",
    "_start:",
    "xor ebp, ebp",
    "mov rdi, rsp",
    "and rsp, -16",
    "call odjkbar_entry",
    "ud2",
);

#[no_mangle]
pub unsafe extern "C" fn odjkbar_entry(sp: *const usize) -> ! {
    let argc = *sp;
    ENVP = sp.add(argc + 2) as *const *const u8;
    crate::real_main();
    exit(0)
}

// ---------- аллокатор ----------

const ARENA: usize = 1 << 20;
const MAX_SHIFT: u32 = 16; // до 64 КиБ -- по классам размера, больше -- прямой mmap
const NCLASS: usize = (MAX_SHIFT - 4 + 1) as usize;
const PAGE: usize = 4096;

struct HeapInner {
    free: [*mut u8; NCLASS],
    cur: usize,
    end: usize,
}

pub struct Heap(UnsafeCell<HeapInner>);

// Бар однопоточный: потоков нет вовсе.
unsafe impl Sync for Heap {}

impl Heap {
    pub const fn new() -> Self {
        Heap(UnsafeCell::new(HeapInner { free: [core::ptr::null_mut(); NCLASS], cur: 0, end: 0 }))
    }
}

fn class_of(l: &Layout) -> Option<usize> {
    let sz = l.size().max(l.align()).max(16).next_power_of_two();
    if sz > (1usize << MAX_SHIFT) { None } else { Some((sz.trailing_zeros() - 4) as usize) }
}

fn map_len(size: usize) -> usize {
    (size + PAGE - 1) & !(PAGE - 1)
}

unsafe fn map_anon(len: usize) -> *mut u8 {
    // PROT_READ|PROT_WRITE = 3, MAP_PRIVATE|MAP_ANONYMOUS = 0x22
    match res(sc(SYS_MMAP, 0, len, 3, 0x22, usize::MAX, 0)) {
        Ok(p) => p as *mut u8,
        Err(_) => core::ptr::null_mut(),
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.align() > PAGE {
            return core::ptr::null_mut();
        }
        let h = &mut *self.0.get();
        let Some(c) = class_of(&l) else { return map_anon(map_len(l.size())) };
        let head = h.free[c];
        if !head.is_null() {
            h.free[c] = *(head as *mut *mut u8);
            return head;
        }
        let sz = 16usize << c;
        let al = sz.min(PAGE);
        let mut p = (h.cur + al - 1) & !(al - 1);
        if h.cur == 0 || p + sz > h.end {
            let base = map_anon(ARENA);
            if base.is_null() {
                return base;
            }
            h.cur = base as usize;
            h.end = h.cur + ARENA;
            p = h.cur;
        }
        h.cur = p + sz;
        p as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, l: Layout) {
        let h = &mut *self.0.get();
        match class_of(&l) {
            Some(c) => {
                // большие свободные блоки отдаём ядру: страницы выпадут из RSS, адрес остаётся в списке
                let sz = 16usize << c;
                if sz >= PAGE {
                    sc(SYS_MADVISE, ptr as usize, sz, 4 /* MADV_DONTNEED */, 0, 0, 0);
                }
                *(ptr as *mut *mut u8) = h.free[c];
                h.free[c] = ptr;
            }
            None => {
                sc(SYS_MUNMAP, ptr as usize, map_len(l.size()), 0, 0, 0, 0);
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let nl = Layout::from_size_align_unchecked(new, l.align());
        match (class_of(&l), class_of(&nl)) {
            (Some(a), Some(b)) if a == b => return ptr,
            (None, None) => {
                // MREMAP_MAYMOVE = 1
                if let Ok(p) = res(sc(SYS_MREMAP, ptr as usize, map_len(l.size()), map_len(new), 1, 0, 0)) {
                    return p as *mut u8;
                }
            }
            _ => {}
        }
        let np = self.alloc(nl);
        if !np.is_null() {
            core::ptr::copy_nonoverlapping(ptr, np, l.size().min(new));
            self.dealloc(ptr, l);
        }
        np
    }
}

#[global_allocator]
static HEAP: Heap = Heap::new();

// ---------- mem*: без libc их никто не даст ----------
// Реализованы на rep-командах: обычный цикл LLVM мог бы заменить вызовом memcpy (бесконечная рекурсия).

/// strlen: компилятор может заменить цикл подсчёта длины вызовом этой функции (зависит от версии rustc).
/// Типы указателей совпадают с libc (c_char, c_void, c_int), иначе rustc предупреждает о подозрительном определении.
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize {
    let s = s as *const u8;
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    asm!("rep movsb", inout("rcx") n => _, inout("rdi") dst => _, inout("rsi") src => _, options(nostack, preserves_flags));
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst as *mut u8, src as *const u8);
    if (d as usize) <= (s as usize) || (d as usize) >= (s as usize) + n {
        asm!("rep movsb", inout("rcx") n => _, inout("rdi") d => _, inout("rsi") s => _, options(nostack, preserves_flags));
    } else {
        asm!(
            "std", "rep movsb", "cld",
            inout("rcx") n => _, inout("rdi") d.add(n).wrapping_sub(1) => _, inout("rsi") s.add(n).wrapping_sub(1) => _,
            options(nostack),
        );
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    asm!("rep stosb", inout("rcx") n => _, inout("rdi") dst => _, in("al") c as u8, options(nostack, preserves_flags));
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    if n == 0 {
        return 0;
    }
    let above: u8;
    let below: u8;
    asm!(
        "repe cmpsb",
        "seta {a}",
        "setb {b}",
        a = out(reg_byte) above,
        b = out(reg_byte) below,
        inout("rcx") n => _, inout("rsi") a => _, inout("rdi") b => _,
        options(nostack),
    );
    above as c_int - below as c_int
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    memcmp(a, b, n)
}

// ---------- вывод ошибок и паника ----------

pub struct Stderr;

impl core::fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_all(2, s.as_bytes());
        Ok(())
    }
}

pub fn die(msg: &str) -> ! {
    write_all(2, msg.as_bytes());
    write_all(2, b"\n");
    exit(1)
}

pub fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let _ = writeln!(Stderr, "odjkbar: panic: {info}");
    exit(101)
}

// Готовая библиотека alloc собрана с раскруткой стека и ссылается на эти два символа.
// При panic = "abort" они недостижимы; заглушки нужны только компоновщику.
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    exit(134)
}
