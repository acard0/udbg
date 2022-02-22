
mod bp;
mod event;
mod shell;
pub mod pdbfile;

// TODO:
#[cfg(not(target_os="macos"))]
mod tests;

use core::ops::Deref;
use std::cell::Cell;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Weak};
pub use std::io::{Error as IoError, ErrorKind, Result as IoResult};

#[cfg(windows)]
use winapi::um::winnt::{PCONTEXT, PEXCEPTION_RECORD, EXCEPTION_POINTERS};

pub use crate::error::*;
pub use crate::os::udbg::*;
pub use {bp::*, shell::*, event::*};
use crate::{*, regs::*, sym::*};

#[cfg(target_arch = "x86_64")]
pub const UDBG_ARCH: u32 = ARCH_X64;
#[cfg(target_arch = "x86")]
pub const UDBG_ARCH: u32 = ARCH_X86;
#[cfg(target_arch = "arm")]
pub const UDBG_ARCH: u32 = ARCH_ARM;
#[cfg(target_arch = "aarch64")]
pub const UDBG_ARCH: u32 = ARCH_ARM64;

pub const MF_IMAGE: u32 = 1 << 0;
pub const MF_MAP: u32 = 1 << 1;
pub const MF_PRIVATE: u32 = 1 << 2;
pub const MF_SECTION: u32 = 1 << 3;
pub const MF_STACK: u32 = 1 << 4;
pub const MF_HEAP: u32 = 1 << 5;
pub const MF_PEB: u32 = 1 << 6;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum UDbgStatus {
    Idle,
    Opened,
    Attached,
    Paused,
    Running,
    Ended,
}

#[derive(Clone)]
pub struct PauseContext {
    pub arch: Cell<u32>,
    pub psize: Cell<usize>,
}

impl Default for PauseContext {
    fn default() -> Self {
        Self {
            arch: UDBG_ARCH.into(),
            psize: core::mem::size_of::<usize>().into()
        }
    }
}

impl PauseContext {
    pub fn update(&self, arch: u32) {
        self.arch.set(arch);
        match arch {
            ARCH_X86 | ARCH_ARM => self.psize.set(4),
            ARCH_X64  | ARCH_ARM64 => self.psize.set(8),
            _ => {}
        };
    }
}

#[derive(Clone, Serialize)]
pub struct UDbgBase {
    pub pid: Cell<pid_t>,
    pub event_tid: Cell<tid_t>,
    pub event_pc: Cell<usize>,
    pub image_path: String,
    pub image_base: usize,
    pub arch: &'static str,
    #[cfg(windows)]
    pub wow64: Cell<bool>,
    #[serde(skip)]
    pub flags: Cell<UFlags>,
    #[serde(skip)]
    pub context: PauseContext,
    #[serde(skip)]
    pub status: Cell<UDbgStatus>,
}

impl Default for UDbgBase {
    fn default() -> Self {
        Self {
            image_path: "".into(),
            image_base: 0,
            context: Default::default(),
            event_pc: Default::default(),
            event_tid: Default::default(),
            pid: Cell::new(0),
            flags: Default::default(),
            #[cfg(windows)]
            wow64: Cell::new(false),
            arch: std::env::consts::ARCH,
            status: Cell::new(udbg::UDbgStatus::Attached),
        }
    }
}

impl UDbgBase {
    #[inline]
    pub fn is_ptr32(&self) -> bool {
        self.ptrsize() == 4
    }

    #[inline]
    pub fn ptrsize(&self) -> usize {
        self.context.psize.get()
    }

    pub fn update_arch(&self, arch: u32) {
        if arch == self.context.arch.get() { return; }
        self.context.update(arch);
        udbg_ui().update_arch(arch);
    }

    pub fn is_opened(&self) -> bool {
        self.status.get() == UDbgStatus::Opened
    }

    pub fn is_paused(&self) -> bool {
        self.status.get() == UDbgStatus::Paused
    }

    pub fn check_opened(&self) -> UDbgResult<()> {
        if self.is_opened() { Err(UDbgError::NotSupport) } else { Ok(()) }
    }

    #[inline(always)]
    pub fn undec_sym(&self, sym: &str) -> Option<String> {
        undecorate_symbol(sym, self.flags.get())
    }
}

#[repr(C)]
#[derive(Serialize, Deserialize)]
pub struct UiThread {
    pub tid: u32,
    pub entry: usize,
    pub teb: usize,
    pub name: Arc<str>,
    pub status: Arc<str>,
    pub priority: Arc<str>,
}

#[repr(C)]
#[derive(Serialize, Deserialize)]
pub struct UiMemory {
    pub base: usize,
    pub size: usize,
    pub flags: u32,     // MF_*
    #[serde(rename="type")]
    pub type_: Arc<str>,
    pub protect: Arc<str>,
    pub usage: Arc<str>,
    #[cfg(windows)]
    pub alloc_base: usize,
}

#[repr(C)]
#[derive(Serialize, Deserialize)]
pub struct UiHandle {
    pub ty: u32,
    pub handle: usize,
    pub type_name: String,
    pub name: String,
}

pub struct ThreadData {
    pub tid: tid_t,
    pub wow64: bool,
    #[cfg(windows)]
    pub handle: Handle,
    #[cfg(target_os="macos")]
    pub handle: crate::process::ThreadAct,
}

#[cfg(windows)]
pub type ThreadContext = winapi::um::winnt::CONTEXT;
#[cfg(windows)]
pub type ThreadContext32 = super::regs::CONTEXT32;

pub trait UDbgThread: Deref<Target=ThreadData> + GetProp {
    fn name(&self) -> Arc<str> { "".into() }
    fn status(&self) -> Arc<str> { "".into() }

    /// https://docs.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getthreadpriority#return-value
    #[cfg(windows)]
    fn priority(&self) -> Option<i32> { None }

    #[cfg(not(windows))]
    fn priority(&self) -> Arc<str> { "".into() }

    fn suspend(&self) -> IoResult<i32> { Err(ErrorKind::Unsupported.into()) }
    fn resume(&self) -> IoResult<u32> { Err(ErrorKind::Unsupported.into()) }
    #[cfg(windows)]
    fn get_context(&self, cx: &mut ThreadContext) -> IoResult<()> {
        Err(IoError::from(ErrorKind::Unsupported))
    }
    #[cfg(windows)]
    fn set_context(&self, cx: &ThreadContext) -> IoResult<()> {
        Err(IoError::from(ErrorKind::Unsupported))
    }
    #[cfg(windows)]
    fn get_context32(&self, cx: &mut ThreadContext32) -> IoResult<()> {
        Err(IoError::from(ErrorKind::Unsupported))
    }
    #[cfg(windows)]
    fn set_context32(&self, cx: &ThreadContext32) -> IoResult<()> {
        Err(IoError::from(ErrorKind::Unsupported))
    }
    #[cfg(windows)]
    fn teb(&self) -> Option<usize> { None }
    /// start address
    #[cfg(windows)]
    fn entry(&self) -> usize { 0 }
    fn last_error(&self) -> Option<u32> { None }
}

pub trait UDbgBreakpoint {
    fn get_id(&self) -> BpID;
    fn address(&self) -> usize;
    fn enabled(&self) -> bool;
    fn get_type(&self) -> BpType;
    /// count of this breakpoint hitted
    fn hit_count(&self) -> usize;
    /// set count of the to be used,
    /// when hit_count() > this count, bp will be delete
    fn set_count(&self, count: usize);
    /// set the which can hit the bp. if tid == 0, all thread used
    fn set_hit_thread(&self, tid: tid_t);
    /// current tid setted by set_hit_thread()
    fn hit_tid(&self) -> tid_t;
    /// original bytes written by software breakpoint
    fn origin_bytes<'a>(&'a self) -> Option<&'a [u8]>;

    fn enable(&self, enable: bool) -> UDbgResult<()>;
    fn remove(&self) -> UDbgResult<()>;
}

/// 表示一个目标(进程)的模块符号管理器
pub trait UDbgSymMgr {
    /// 查找address所处的模块，address可以是模块基址，也可以是模块范围内的任意地址
    fn find_module(&self, address: usize) -> Option<Arc<dyn UDbgModule>>;
    /// 根据模块名查找模块
    fn get_module(&self, name: &str) -> Option<Arc<dyn UDbgModule>>;
    /// 枚举模块
    fn enum_module<'a>(&'a self) -> Box<dyn Iterator<Item=Arc<dyn UDbgModule + 'a>> + 'a>;
    /// 枚举符号
    fn enum_symbol<'a>(&'a self, pat: Option<&str>) -> UDbgResult<Box<dyn Iterator<Item=sym::Symbol>+'a>> {
        Err(UDbgError::NotSupport)
    }
    /// 移除模块及其符号，通过基址定位模块
    fn remove(&self, address: usize);
    #[cfg(windows)]
    fn check_load_module(&self, read: &dyn ReadMemory, base: usize, size: usize, path: &str, file: winapi::um::winnt::HANDLE) -> bool { false }
}

pub trait UDbgEngine {
    fn enum_process(&self) -> Box<dyn Iterator<Item=PsInfo>> {
        enum_psinfo()
    }

    fn open(&self, base: UDbgBase, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>>;

    fn open_self(&self, base: UDbgBase) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        self.open(base, std::process::id() as _)
    }

    fn attach(&self, base: UDbgBase, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>>;

    fn create(&self, base: UDbgBase, path: &str, cwd: Option<&str>, args: &[&str]) -> UDbgResult<Arc<dyn UDbgAdaptor>>;
}

pub type UDbgCallback<'a> = dyn FnMut(UEvent) -> UserReply + 'a;

#[cfg(windows)]
pub trait AdaptorSpec {
    fn handle(&self) -> HANDLE { core::ptr::null_mut() }

    fn exception_context(&self) -> UDbgResult<PCONTEXT> {
        Err(UDbgError::NotSupport)
    }

    fn exception_record(&self) -> UDbgResult<PEXCEPTION_RECORD> {
        Err(UDbgError::NotSupport)
    }

    fn exception_pointers(&self) -> UDbgResult<EXCEPTION_POINTERS> {
        Ok(EXCEPTION_POINTERS {
            ExceptionRecord: self.exception_record()?,
            ContextRecord: self.exception_context()?,
        })
    }
}

#[cfg(not(windows))]
pub trait AdaptorSpec {
}

pub trait GetProp {
    fn get_prop(&self, key: &str) -> UDbgResult<serde_value::Value> {
        Err(UDbgError::NotSupport)
    }
}

impl<T> GetProp for T {
    default fn get_prop(&self, key: &str) -> UDbgResult<serde_value::Value> {
        Err(UDbgError::NotSupport)
    }
}

pub trait TargetControl {
    fn detach(&self) -> UDbgResult<()>;
    fn breakk(&self) -> UDbgResult<()> { Err(UDbgError::NotSupport) }
    fn kill(&self) -> UDbgResult<()>;
    fn suspend(&self) -> UDbgResult<()> { Ok(()) }
    fn resume(&self) -> UDbgResult<()> { Ok(()) }
}

pub trait UDbgAdaptor: Send + Sync + GetProp + TargetMemory + TargetControl + AdaptorSpec + 'static {
    fn base(&self) -> &UDbgBase;

    // memory infomation
    fn get_memory_map(&self) -> Vec<UiMemory>;

    // thread infomation
    fn get_thread_context(&self, tid: u32) -> Option<Registers> { None }
    fn enum_thread<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=tid_t>+'a>>;
    fn open_thread(&self, tid: tid_t) -> UDbgResult<Box<dyn UDbgThread>> { Err(UDbgError::NotSupport) }
    fn open_all_thread(&self) -> Vec<Box<dyn UDbgThread>> {
        self.enum_thread().map(
            |iter| iter.filter_map(|tid| self.open_thread(tid).ok()).collect::<Vec<_>>()
        ).unwrap_or_default()
    }

    // breakpoint
    fn add_bp(&self, opt: BpOpt) -> UDbgResult<Arc<dyn UDbgBreakpoint>> { Err(UDbgError::NotSupport) }
    fn get_bp<'a>(&'a self, id: BpID) -> Option<Arc<dyn UDbgBreakpoint + 'a>> { None }
    fn get_bp_by_address<'a>(&'a self, a: usize) -> Option<Arc<dyn UDbgBreakpoint + 'a>> {
        self.get_bp(a as BpID)
    }
    fn get_bp_list(&self) -> Vec<BpID> { vec![] }
    fn get_breakpoints<'a>(&'a self) -> Vec<Arc<dyn UDbgBreakpoint + 'a>> {
        self.get_bp_list().into_iter().filter_map(|id| self.get_bp(id)).collect()
    }

    // symbol infomation
    fn symbol_manager(&self) -> Option<&dyn UDbgSymMgr> { None }
    fn enum_module<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=Arc<dyn UDbgModule+'a>>+'a>> {
        Ok(self.symbol_manager().ok_or(UDbgError::NotSupport)?.enum_module())
    }
    fn find_module(&self, module: usize) -> Option<Arc<dyn UDbgModule>> {
        self.symbol_manager()?.find_module(module)
    }
    fn get_module(&self, module: &str) -> Option<Arc<dyn UDbgModule>> {
        self.symbol_manager()?.get_module(module)
    }
    fn get_address_by_symbol(&self, symbol: &str) -> Option<usize> {
        let (left, right) = symbol.find('!')
        .map(|pos| ((&symbol[..pos]).trim(), (&symbol[pos + 1..]).trim()))
        .unwrap_or((symbol, ""));
        if right.is_empty() {
            if let Some(m) = self.get_module(left) {
                // as module name
                Some(m.data().base)
            } else {
                // as symbol name
                self.enum_module().ok()?.filter_map(|m| m.get_symbol(left).map(|s| s.offset as usize + m.data().base)).next()
            }
        } else {
            let m = self.get_module(left)?;
            let d = m.data();
            if right == "$entry" { return Some(d.entry + d.base); }
            m.get_symbol(right).map(|s| d.base + s.offset as usize)
        }
    }
    fn get_symbol(&self, addr: usize, max_offset: usize) -> Option<SymbolInfo> {
        self.find_module(addr).and_then(|m| {
            let d = m.data();
            let offset = addr - d.base;
            m.find_symbol(offset, max_offset).and_then(|s| {
                let soffset = offset - s.offset as usize;
                if soffset > max_offset { return None; }
                Some(SymbolInfo {
                    mod_base: d.base, offset: soffset, module: d.name.clone(),
                    symbol: if let Some(n) = self.base().undec_sym(s.name.as_ref()) { n.into() } else { s.name }
                })
            }).or_else(|| /* if let Some((b, e, _)) = m.find_function(offset) {
                Some(SymbolInfo { mod_base: d.base, offset: offset - b as usize, module: d.name.clone(), symbol: format!("${:x}", d.base + b as usize).into() })
            } else */ if max_offset > 0 {
                Some(SymbolInfo { mod_base: d.base, offset, module: d.name.clone(), symbol: "".into() })
            } else { None })
        })
    }

    fn enum_handle<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item=UiHandle>+'a>> { Err(UDbgError::NotSupport) }
    fn get_registers<'a>(&'a self) -> UDbgResult<&'a mut dyn UDbgRegs> {
        Err(UDbgError::NotSupport)
    }

    fn except_param(&self, i: usize) -> Option<usize> { None }
    fn do_cmd(&self, cmd: &str) -> UDbgResult<()> { Err(UDbgError::NotSupport) }

    fn event_loop<'a>(&self, callback: &mut UDbgCallback<'a>) -> UDbgResult<()> { Err(UDbgError::NotSupport) }
}

pub trait UDbgDebug: UDbgAdaptor {}

pub trait AdaptorUtil: UDbgAdaptor {
    fn read_ptr(&self, a: usize) -> Option<usize> {
        if self.base().is_ptr32() {
            self.read_value::<u32>(a).map(|r| r as usize)
        } else {
            self.read_value::<u64>(a).map(|r| r as usize)
        }
    }

    fn write_ptr(&self, a: usize, p: usize) -> Option<usize> {
        if self.base().is_ptr32() {
            self.write_value(a, &(p as u32))
        } else {
            self.write_value(a, &(p as u64))
        }
    }

    fn get_reg(&self, r: &str) -> UDbgResult<CpuReg> {
        self.get_registers()?.get_reg(get_regid(r).ok_or(UDbgError::InvalidRegister)?).ok_or(UDbgError::InvalidRegister)
    }

    fn set_reg(&self, r: &str, val: CpuReg) -> UDbgResult<()> {
        self.get_registers()?.set_reg(get_regid(r).ok_or(UDbgError::InvalidRegister)?, val);
        Ok(())
    }

    fn parse_address(&self, symbol: &str) -> Option<usize> {
        let (mut left, right) = match symbol.find('+') {
            Some(pos) => ((&symbol[..pos]).trim(), Some((&symbol[pos + 1..]).trim())),
            None => (symbol.trim(), None),
        };

        let mut val = if let Ok(val) = self.get_reg(left) {
            val.as_int()
        } else {
            if left.starts_with("0x") || left.starts_with("0X") {
                left = &left[2..];
            }
            if let Ok(address) = usize::from_str_radix(left, 16) {
                address
            } else { self.get_address_by_symbol(left)? }
        };

        if let Some(right) = right { val += self.parse_address(right)?; }

        Some(val)
    }

    fn get_symbol_(&self, addr: usize, o: Option<usize>) -> Option<SymbolInfo> {
        UDbgAdaptor::get_symbol(self, addr, o.unwrap_or(0x100))
    }

    fn get_symbol_string(&self, addr: usize) -> Option<String> {
        self.get_symbol_(addr, None).map(|s| s.to_string(addr))
    }

    fn get_symbol_module_info(&self, addr: usize) -> Option<String> {
        self.find_module(addr).map(|m| {
            let data = m.data();
            let offset = addr - data.base;
            if offset > 0 { format!("{}+{:x}", data.name, offset) } else { data.name.to_string() }
        })
    }

    fn get_main_module<'a>(&'a self) -> Option<Arc<dyn UDbgModule + 'a>> {
        let base = self.base();
        if base.image_base > 0 {
            self.find_module(base.image_base)
        } else {
            let image_path = &self.base().image_path;
            for m in self.enum_module().ok()? {
                let path = m.data().path.clone();
                #[cfg(windows)] {
                    if path.eq_ignore_ascii_case(&image_path) { return Some(m); }
                }
                #[cfg(not(windows))] {
                    if path.as_ref() == image_path { return Some(m); }
                }
            }
            None
        }
    }

    #[cfg(not(windows))]
    fn get_module_entry(&self, base: usize) -> usize {
        use goblin::elf32::header::Header as Header32;
        use goblin::elf64::header::Header as Header64;

        let mut buf = vec![0u8; core::mem::size_of::<Header64>()];
        if let Some(header) = self.read_memory(base, &mut buf) {
            base + Header64::parse(header).ok().map(|h| h.e_entry as usize)
            .or_else(|| Header32::parse(header).map(|h| h.e_entry as usize).ok()).unwrap_or_default()
        } else { 0 }
    }

    #[cfg(windows)]
    fn get_module_entry(&self, base: usize) -> usize {
        self.read_nt_header(base).map(|(nt, _)| base + nt.OptionalHeader.AddressOfEntryPoint as usize).unwrap_or(0)
    }

    fn detect_string(&self, a: usize, max: usize) -> Option<(bool, String)> {
        fn ascii_count(s: &str) -> usize {
            let mut r = 0usize;
            for c in s.chars() { if c.is_ascii() { r += 1; } }
            return r;
        }
        // guess string
        #[cfg(windows)]
        let ws = self.read_wstring(a, max);
        #[cfg(not(windows))]
        let ws: Option<String> = None;
        if let Some(s) = self.read_utf8(a, max) {
            return if let Some(ws) = ws {
                if ascii_count(&s) > ascii_count(&ws) {
                    Some((false, s))
                } else { Some((true, ws)) }
            } else { Some((false, s)) };
        } None
    }

    #[inline]
    fn pid(&self) -> pid_t { self.base().pid.get() }

    fn loop_event<U: std::future::Future<Output=()> + 'static, F: FnOnce(Arc<Self>, UEventState)->U>(self: Arc<Self>, callback: F) {
        let state = UEventState::new();
        let mut fetcher = ReplyFetcher::new(Box::pin(callback(self.clone(), state.clone())), state);
        self.event_loop(&mut |event| {
            fetcher.fetch(event).unwrap_or(UserReply::Run(false))
        });
        fetcher.fetch(None);
    }
}
impl<'a, T: UDbgAdaptor + ?Sized + 'a> AdaptorUtil for T {}

#[cfg(any(target_arch="x86", target_arch="x86_64"))]
pub trait AdaptorArchUtil: UDbgAdaptor {
    fn disasm(&self, address: usize) -> Option<iced_x86::Instruction> {
        use iced_x86::{Decoder, DecoderOptions, Instruction};

        let buffer = self.read_bytes(address, MAX_INSN_SIZE);
        let mut decoder = Decoder::new(if self.base().is_ptr32() { 32 } else { 64 }, buffer.as_slice(), DecoderOptions::NONE);
        let mut insn = Instruction::default();
        if decoder.can_decode() {
            decoder.decode_out(&mut insn);
            Some(insn)
        } else { None }
    }

    #[inline(always)]
    fn check_call(&self, address: usize) -> Option<usize> {
        use iced_x86::Mnemonic::*;

        self.disasm(address).and_then(|insn|
            if matches!(insn.mnemonic(), Call | Syscall | Sysenter) || insn.has_rep_prefix() {
                Some(address + insn.len())
            } else { None }
        )
    }
}

#[cfg(any(target_arch="arm", target_arch="aarch64"))]
pub trait AdaptorArchUtil: UDbgAdaptor {
    #[inline(always)]
    fn check_call(&self, address: usize) -> Option<usize> {
        todo!();
    }
}

impl<'a, T: UDbgAdaptor + ?Sized + 'a> AdaptorArchUtil for T {}

use crate::range::RangeValue;

impl RangeValue for UiMemory {
    #[inline]
    fn as_range(&self) -> core::ops::Range<usize> { self.base..self.base+self.size }
}

impl RangeValue for MemoryPage {
    #[inline]
    fn as_range(&self) -> core::ops::Range<usize> { self.base..self.base+self.size }
}

pub fn to_weak<T: ?Sized>(t: &T) -> Weak<T> {
    unsafe {
        let t = Arc::from_raw(t);
        let result = Arc::downgrade(&t);
        Arc::into_raw(t);
        result
    }
}

#[derive(Clone)]
pub struct Breakpoint {
    pub address: usize,
    pub enabled: Cell<bool>,
    pub temp: Cell<bool>,
    pub bp_type: InnerBpType,
    pub hit_count: Cell<usize>,
    pub hit_tid: Option<tid_t>,

    pub target: Weak<dyn UDbgAdaptor>,
    pub common: *const crate::os::udbg::CommonAdaptor,
}

impl Breakpoint {
    pub fn get_hwbp_len(&self) -> Option<usize> {
        if let InnerBpType::Hard(info) = self.bp_type {
            Some(match info.len as crate::reg_t {
                LEN_1 => 1,
                LEN_2 => 2,
                LEN_4 => 4,
                LEN_8 => 8,
                _ => 0,
            })
        } else { None }
    }

    #[inline]
    pub fn is_hard(&self) -> bool {
        if let InnerBpType::Hard {..} = self.bp_type { true } else { false }
    }

    #[inline]
    pub fn is_soft(&self) -> bool {
        if let InnerBpType::Soft(_) = self.bp_type { true } else { false }
    }

    #[inline]
    pub fn is_table(&self) -> bool {
        if let InnerBpType::Table {..} = self.bp_type { true } else { false }
    }

    #[inline]
    pub fn hard_index(&self) -> Option<usize> {
        if let InnerBpType::Hard(info) = self.bp_type {
            Some(info.index as usize)
        } else { None }
    }
}

impl UDbgBreakpoint for Breakpoint {
    fn get_id(&self) -> BpID { self.address as BpID }
    fn address(&self) -> usize { self.address }
    fn enabled(&self) -> bool { self.enabled.get() }
    fn get_type(&self) -> BpType {
        match self.bp_type {
            InnerBpType::Soft {..} => BpType::Soft,
            InnerBpType::Table {..} => BpType::Table,
            InnerBpType::Hard(info) => BpType::Hwbp(info.rw.into(), info.len),
        }
    }
    /// count of this breakpoint hitted
    fn hit_count(&self) -> usize { self.hit_count.get() }
    /// set count of the to be used,
    /// when hit_count() > this count, bp will be delete
    fn set_count(&self, count: usize) {}
    /// set the which can hit the bp. if tid == 0, all thread used
    fn set_hit_thread(&self, tid: tid_t) {}
    /// current tid setted by set_hit_thread()
    fn hit_tid(&self) -> tid_t { 0 }

    fn origin_bytes<'a>(&'a self) -> Option<&'a [u8]> {
        if let InnerBpType::Soft(raw) = &self.bp_type {
            Some(raw)
        } else { None }
    }

    fn enable(&self, enable: bool) -> UDbgResult<()> {
        let t = self.target.upgrade().ok_or(UDbgError::NoTarget)?;
        unsafe {
            let common = self.common.as_ref().unwrap();
            #[cfg(windows)]
            common.enable_breadpoint(t.as_ref(), self, enable)?;
            #[cfg(not(windows))]
            common.enable_breadpoint(self, enable)?;
            Ok(())
        }
    }

    fn remove(&self) -> UDbgResult<()> {
        let t = self.target.upgrade().ok_or(UDbgError::NoTarget)?;
        unsafe {
            let common = self.common.as_ref().unwrap();
            self.enable(false);
            #[cfg(windows)]
            common.remove_breakpoint(t.as_ref(), self);
            #[cfg(not(windows))] {
                if let Some(bp) = common.bp_map.write().remove(&self.get_id()) {
                    if let InnerBpType::Hard(_) = bp.bp_type {
                        // TODO: nix
                    }
                }
            }
            Ok(())
        }
    }
}