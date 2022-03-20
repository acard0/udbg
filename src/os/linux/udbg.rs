use super::*;
use crate::elf::*;
use crate::os::tid_t;
use crate::range::RangeValue;
use crate::register::*;

use anyhow::Context;
use goblin::elf::sym::Sym;
use nix::sys::ptrace::Options;
use nix::sys::wait::waitpid;
use parking_lot::RwLock;
use procfs::process::{Stat as ThreadStat, Task};
use serde_value::Value;
use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, HashSet};
use std::mem::{size_of, transmute, zeroed};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::{ptrace, signal::Signal, wait::*};

cfg_if! {
    if #[cfg(target_os = "android")] {
        const PTRACE_INTERRUPT: c_uint = 0x4207;
        const PTRACE_SEIZE: c_uint = 0x4206;
    }
}

#[inline]
unsafe fn mutable<T: Sized>(t: &T) -> &mut T {
    transmute(transmute::<_, usize>(t))
}

pub struct ElfSymbol {
    pub sym: Sym,
    pub name: Arc<str>,
}

impl Deref for ElfSymbol {
    type Target = Sym;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.sym
    }
}

impl From<ElfSym<'_>> for ElfSymbol {
    fn from(s: ElfSym<'_>) -> Self {
        ElfSymbol {
            sym: s.sym,
            name: s.name.into(),
        }
    }
}

#[derive(Deref)]
pub struct NixThread {
    #[deref]
    base: ThreadData,
    stat: ThreadStat,
}

impl GetProp for NixThread {}

impl UDbgThread for NixThread {
    fn name(&self) -> Arc<str> {
        self.stat.comm.as_str().into()
    }
    fn status(&self) -> Arc<str> {
        self.stat.state.to_string().into()
    }
    fn priority(&self) -> Arc<str> {
        format!("{}", self.stat.priority).into()
    }
}

#[inline(always)]
fn to_symbol(s: ElfSym) -> Symbol {
    let flags = if s.is_function() {
        SymbolFlags::FUNCTION
    } else {
        SymbolFlags::NONE
    };
    Symbol {
        offset: s.st_value as u32,
        name: s.name.into(),
        flags: flags.bits(),
        len: s.st_size as u32,
        type_id: 0,
    }
}

impl SymbolsData {
    fn from_elf(path: &str) -> Self {
        let mut this = Self::default();
        this.load(path);
        this
    }

    fn load(&mut self, path: &str) -> Result<(), String> {
        let map = Utils::mapfile(path.as_ref()).ok_or("map failed")?;
        let e = ElfHelper::parse(&map).ok_or("parse failed")?;
        let mut push_symbol = |s: ElfSym| {
            if s.name.starts_with("$x.") {
                return;
            }
            self.exports
                .entry(s.offset())
                .or_insert_with(|| to_symbol(s));
        };
        e.enum_symbol().for_each(&mut push_symbol);
        e.enum_export().for_each(&mut push_symbol);
        Ok(())
    }
}

struct TimeCheck {
    last: Cell<Instant>,
    pub duration: Cell<Duration>,
}

impl TimeCheck {
    pub fn new(duration: Duration) -> Self {
        Self {
            last: Instant::now().checked_sub(duration).unwrap().into(),
            duration: duration.into(),
        }
    }

    pub fn check(&self, mut callback: impl FnMut()) {
        if self.last.get().elapsed() > self.duration.get() {
            callback();
            self.last.set(Instant::now());
        }
    }
}

pub struct CommonAdaptor {
    base: TargetBase,
    ps: Process,
    symgr: SymbolManager<NixModule>,
    pub bp_map: RwLock<HashMap<BpID, Arc<Breakpoint>>>,
    regs: UnsafeCell<user_regs_struct>,
    threads: RwLock<HashSet<tid_t>>,
    tc_module: TimeCheck,
    tc_memory: TimeCheck,
    mem_pages: RwLock<Vec<MemoryPage>>,
    detaching: Cell<bool>,
    waiting: Cell<bool>,
    pub trace_opts: Options,
}

impl CommonAdaptor {
    fn new(ps: Process) -> Self {
        const TIMEOUT: Duration = Duration::from_secs(5);

        let mut base = TargetBase::default();
        let mut trace_opts =
            Options::PTRACE_O_EXITKILL | Options::PTRACE_O_TRACECLONE | Options::PTRACE_O_TRACEEXEC;
        if udbg_ui().get_config::<bool>("trace_fork").unwrap_or(false) {
            trace_opts |= Options::PTRACE_O_TRACEVFORK | Options::PTRACE_O_TRACEFORK;
        }

        base.pid.set(ps.pid());
        base.image_path = ps.image_path().unwrap_or_default();
        Self {
            base,
            ps,
            regs: unsafe { zeroed() },
            bp_map: RwLock::new(HashMap::new()),
            symgr: SymbolManager::<NixModule>::new("".into()),
            tc_module: TimeCheck::new(Duration::from_secs(10)),
            tc_memory: TimeCheck::new(Duration::from_secs(10)),
            mem_pages: RwLock::new(Vec::new()),
            threads: RwLock::new(HashSet::new()),
            trace_opts,
            waiting: Cell::new(false),
            detaching: Cell::new(false),
        }
    }

    fn update_memory_page(&self) -> IoResult<()> {
        *self.mem_pages.write() = self.ps.enum_memory()?.collect::<Vec<_>>();
        Ok(())
    }

    fn update_memory_page_check_time(&self) {
        self.tc_memory.check(|| {
            self.update_memory_page();
        });
    }

    // fn update_thread(&self) {
    //     let ts = unsafe { mutable(&self.threads) };
    //     let mut maps: HashSet<pid_t> = HashSet::new();
    //     for tid in process_tasks(self.pid) {
    //         if !ts.contains(&tid) {
    //             self.dbg.map(|d| d.thread_create(tid as u32));
    //         }
    //         maps.insert(tid);
    //     }
    //     *ts = maps;
    // }

    fn module_name<'a>(&self, name: &'a str) -> &'a str {
        let tv = trim_ver(name);
        let te = trim_allext(name);
        let base = self.symgr.base.read();
        if tv.len() < te.len() && !base.contains(tv) {
            return tv;
        }
        if !base.contains(te) {
            return te;
        }
        let te = trim_lastext(name);
        if !base.contains(te) {
            return te;
        }
        name
    }

    fn update_module(&self) -> IoResult<()> {
        use goblin::elf::header::header32::Header as Header32;
        use goblin::elf::header::header64::Header as Header64;
        use std::io::Read;

        // self.md.write().clear();
        for m in self.ps.enum_module()? {
            if self.find_module(m.base).is_some()
                || m.name.ends_with(".oat")
                || m.name.ends_with(".apk")
            {
                continue;
            }
            let name = self.module_name(&m.name);

            // TODO: use memory data
            let mut f = match File::open(m.path.as_ref()) {
                Ok(f) => f,
                Err(_) => {
                    error!("open module file: {}", m.path);
                    continue;
                }
            };
            let mut buf: Header64 = unsafe { std::mem::zeroed() };
            if f.read_exact(buf.as_mut_byte_array()).is_err() {
                error!("read file: {}", m.path);
                continue;
            }

            let arch = match ElfHelper::arch_name(buf.e_machine) {
                Some(a) => a,
                None => {
                    error!("error e_machine: {} {}", buf.e_machine, m.path);
                    continue;
                }
            };

            let entry = match arch {
                "arm64" | "x86_64" => buf.e_entry as usize,
                "x86" | "arm" => unsafe { transmute::<_, &Header32>(&buf).e_entry as usize },
                a => {
                    error!("error arch: {}", a);
                    continue;
                }
            };

            let base = m.base;
            let path = m.path.clone();
            self.symgr.base.write().add(NixModule {
                data: ModuleData {
                    base,
                    size: m.size,
                    arch,
                    entry,
                    user_module: false.into(),
                    name: name.into(),
                    path: path.clone(),
                },
                loaded: false.into(),
                syms: SymbolsData::from_elf(&path).into(),
            });
            // TODO:
            // self.base.module_load(&path, base);
        }
        Ok(())
    }

    #[inline(always)]
    fn find_module(&self, address: usize) -> Option<Arc<NixModule>> {
        self.symgr.find_module(address)
    }

    #[inline(always)]
    fn bp_exists(&self, id: BpID) -> bool {
        self.bp_map.read().get(&id).is_some()
    }

    pub fn enable_breadpoint(&self, bp: &Breakpoint, enable: bool) -> Result<bool, UDbgError> {
        match bp.bp_type {
            InnerBpType::Soft(origin) => {
                let written = if enable {
                    self.ps.write(bp.address, &BP_INSN)
                } else {
                    self.ps.write(bp.address, &origin)
                };
                if written.unwrap_or(0) > 0 {
                    bp.enabled.set(enable);
                    Ok(enable)
                } else {
                    Err(UDbgError::MemoryError)
                }
            }
            _ => Err(UDbgError::NotSupport),
        }
    }

    fn readv<T: Copy>(&self, address: usize) -> Option<T> {
        unsafe {
            let mut val: T = zeroed();
            let size = size_of::<T>();
            let pdata: *mut u8 = transmute(&mut val);
            let mut data = std::slice::from_raw_parts_mut(pdata, size);
            let readed = self.ps.read(address, &mut data);
            if readed?.len() == size {
                Some(val)
            } else {
                None
            }
        }
    }

    fn update_regs(&self, tid: pid_t) {
        match ptrace::getregs(Pid::from_raw(tid)) {
            Ok(regs) => unsafe {
                *self.regs.get() = regs;
            },
            Err(err) => {
                error!("get_regs failed: {err:?}");
            }
        };
    }

    fn set_regs(&self) -> UDbgResult<()> {
        ptrace::setregs(Pid::from_raw(self.base.event_tid.get()), unsafe {
            *self.regs.get()
        })
        .context("")?;
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn set_reg(&self, r: &str, val: CpuReg) -> UDbgResult<()> {
        let regs = unsafe { mutable(&self.regs) };
        let val = val.as_int() as u64;
        match r {
            "pc" | "_pc" => regs.pc = val,
            "sp" | "_sp" => regs.sp = val,
            "pstate" => regs.pstate = val,
            "x0" => regs.regs[0] = val,
            "x1" => regs.regs[1] = val,
            "x2" => regs.regs[2] = val,
            "x3" => regs.regs[3] = val,
            "x4" => regs.regs[4] = val,
            "x5" => regs.regs[5] = val,
            "x6" => regs.regs[6] = val,
            "x7" => regs.regs[7] = val,
            "x8" => regs.regs[8] = val,
            "x9" => regs.regs[9] = val,
            "x10" => regs.regs[10] = val,
            "x11" => regs.regs[11] = val,
            "x12" => regs.regs[12] = val,
            "x13" => regs.regs[13] = val,
            "x14" => regs.regs[14] = val,
            "x15" => regs.regs[15] = val,
            "x16" => regs.regs[16] = val,
            "x17" => regs.regs[17] = val,
            "x18" => regs.regs[18] = val,
            "x19" => regs.regs[19] = val,
            "x20" => regs.regs[20] = val,
            "x21" => regs.regs[21] = val,
            "x22" => regs.regs[22] = val,
            "x23" => regs.regs[23] = val,
            "x24" => regs.regs[24] = val,
            "x25" => regs.regs[25] = val,
            "x26" => regs.regs[26] = val,
            "x27" => regs.regs[27] = val,
            "x28" => regs.regs[28] = val,
            "x29" => regs.regs[29] = val,
            "x30" => regs.regs[30] = val,
            _ => return Err(UDbgError::InvalidRegister),
        };
        self.set_regs()
    }

    fn wait_event(&self, tb: &mut TraceBuf) -> Option<WaitStatus> {
        self.base.status.set(UDbgStatus::Running);
        self.waiting.set(true);
        let mut status = 0;
        let tid = unsafe { libc::waitpid(-1, &mut status, __WALL | WUNTRACED) };
        // let status = ::nix::sys::wait::waitpid(None, WaitPidFlag::__WALL | WaitPidFlag::__WNOTHREAD | WaitPidFlag::WNOHANG).unwrap();
        self.waiting.set(false);
        self.base.event_tid.set(tid);
        self.base.status.set(UDbgStatus::Paused);

        if tid <= 0 {
            return None;
        }

        let status = WaitStatus::from_raw(Pid::from_raw(tid), status).unwrap();
        println!("[status] {status:?}");
        Some(status)
    }

    fn handle_event(&self, tb: &mut TraceBuf) {}

    fn handle_reply(
        &self,
        this: &dyn UDbgAdaptor,
        mut reply: UserReply,
        tid: pid_t,
        bpid: Option<BpID>,
    ) -> UserReply {
        let mut revert = None;
        let ui = udbg_ui();
        if let Some(bpid) = bpid {
            this.get_bp(bpid).map(|bp| {
                if bp.enabled() {
                    // Disable breakpoint temporarily
                    bp.enable(false);
                    revert = Some(bpid);
                }
            });
        }
        ptrace_step_and_wait(tid);
        // TODO:
        // if let Some(bpid) = revert { this.enable_bp(bpid, true); }

        let mut temp_address: Option<usize> = None;
        match reply {
            UserReply::StepOut => {
                let regs = unsafe { &mut *self.regs.get() };
                temp_address = this.check_call(*regs.ip() as usize);
                if temp_address.is_none() {
                    reply = UserReply::StepIn;
                }
            }
            UserReply::Goto(a) => {
                temp_address = Some(a);
            }
            UserReply::StepIn => {}
            _ => {}
        }

        if let Some(address) = temp_address {
            this.add_bp(BpOpt::int3(address).enable(true).temp(true));
        }

        self.update_regs(tid);
        reply
    }

    fn get_bp_(&self, id: BpID) -> Option<Arc<Breakpoint>> {
        Some(self.bp_map.read().get(&id)?.clone())
    }

    pub fn handle_breakpoint(
        &self,
        this: &dyn UDbgAdaptor,
        tid: pid_t,
        info: &siginfo_t,
        bp: Arc<Breakpoint>,
        tb: &mut TraceBuf,
    ) {
        bp.hit_count.set(bp.hit_count.get() + 1);
        let regs = unsafe { mutable(&self.regs) };
        if bp.temp.get() {
            bp.remove();
        }
        let mut reply = self.handle_reply(
            this,
            tb.call(UEvent::Breakpoint(bp.clone())),
            tid,
            Some(bp.get_id()),
        );
        while reply == UserReply::StepIn {
            reply = self.handle_reply(this, tb.call(UEvent::Step), tid, None);
        }
    }

    fn enum_module<'a>(
        &'a self,
    ) -> UDbgResult<Box<dyn Iterator<Item = Arc<dyn UDbgModule + 'a>> + 'a>> {
        self.update_module();
        Ok(self.symgr.enum_module())
    }

    fn enum_memory<'a>(&'a self) -> Result<Box<dyn Iterator<Item = MemoryPage> + 'a>, UDbgError> {
        self.update_memory_page();
        Ok(Box::new(self.mem_pages.read().clone().into_iter()))
    }

    fn get_memory_map(&self) -> Vec<MemoryPageInfo> {
        self.enum_memory()
            .unwrap()
            .map(|m| {
                let mut flags = 0u32;
                if m.usage.as_ref() == "[heap]" {
                    flags |= MF_HEAP;
                }
                if m.usage.as_ref() == "[stack]" {
                    flags |= MF_STACK;
                }
                MemoryPageInfo {
                    base: m.base,
                    size: m.size,
                    flags,
                    type_: m.type_str().into(),
                    protect: m.protect().into(),
                    usage: m.usage.clone(),
                }
            })
            .collect::<Vec<_>>()
    }

    fn enum_handle<'a>(&'a self) -> Result<Box<dyn Iterator<Item = HandleInfo> + 'a>, UDbgError> {
        use std::os::unix::fs::FileTypeExt;

        Ok(Box::new(
            process_fd(self.ps.pid)
                .ok_or(UDbgError::system())?
                .map(|(id, path)| {
                    let ps = path.to_str().unwrap_or("");
                    let ts = path
                        .metadata()
                        .map(|m| {
                            let ft = m.file_type();
                            if ft.is_fifo() {
                                "FIFO"
                            } else if ft.is_socket() {
                                "Socket"
                            } else if ft.is_block_device() {
                                "Block"
                            } else {
                                "File"
                            }
                        })
                        .unwrap_or_else(|_| {
                            if ps.starts_with("socket:") {
                                "Socket"
                            } else if ps.starts_with("pipe:") {
                                "Pipe"
                            } else {
                                ""
                            }
                        });
                    HandleInfo {
                        ty: 0,
                        handle: id,
                        type_name: ts.to_string(),
                        name: ps.to_string(),
                    }
                }),
        ))
    }

    fn get_regs(&self) -> UDbgResult<Registers> {
        unsafe {
            let mut result: Registers = zeroed();
            self.get_reg("", Some(&mut result))?;
            Ok(result)
        }
    }

    fn regs(&self) -> &mut user_regs_struct {
        unsafe { &mut *self.regs.get() }
    }

    #[cfg(target_arch = "x86_64")]
    fn get_reg(&self, reg: &str, r: Option<&mut Registers>) -> Result<CpuReg, UDbgError> {
        let regs = self.regs();
        if let Some(r) = r {
            r.rax = regs.rax;
            r.rbx = regs.rbx;
            r.rcx = regs.rcx;
            r.rdx = regs.rdx;
            r.rbp = regs.rbp;
            r.rsp = regs.rsp;
            r.rsi = regs.rsi;
            r.rdi = regs.rdi;
            r.r8 = regs.r8;
            r.r9 = regs.r9;
            r.r10 = regs.r10;
            r.r11 = regs.r11;
            r.r12 = regs.r12;
            r.r13 = regs.r13;
            r.r14 = regs.r14;
            r.r15 = regs.r15;
            r.rip = regs.rip;
            r.rflags = regs.eflags as reg_t;
            Ok(0.into())
        } else {
            Ok(CpuReg::Int(match reg {
                "rax" => regs.rax,
                "rbx" => regs.rbx,
                "rcx" => regs.rcx,
                "rdx" => regs.rdx,
                "rbp" => regs.rbp,
                "rsp" | "_sp" => regs.rsp,
                "rsi" => regs.rsi,
                "rdi" => regs.rdi,
                "r8" => regs.r8,
                "r9" => regs.r9,
                "r10" => regs.r10,
                "r11" => regs.r11,
                "r12" => regs.r12,
                "r13" => regs.r13,
                "r14" => regs.r14,
                "r15" => regs.r15,
                "rip" | "_pc" => regs.rip,
                "rflags" => regs.eflags as reg_t,
                _ => return Err(UDbgError::InvalidRegister),
            } as usize))
        }
    }

    #[cfg(target_arch = "arm")]
    fn get_reg(&self, reg: &str, r: Option<&mut Registers>) -> Result<CpuReg, UDbgError> {
        let regs = self.regs();
        if let Some(r) = r {
            *r = unsafe { transmute(*regs) };
            Ok(CpuReg::Int(0))
        } else {
            Ok(CpuReg::Int(match reg {
                "r0" => regs.regs[0],
                "r1" => regs.regs[1],
                "r2" => regs.regs[2],
                "r3" => regs.regs[3],
                "r4" => regs.regs[4],
                "r5" => regs.regs[5],
                "r6" => regs.regs[6],
                "r7" => regs.regs[7],
                "r8" => regs.regs[8],
                "r9" => regs.regs[9],
                "r10" => regs.regs[10],
                "r11" => regs.regs[11],
                "r12" => regs.regs[12],
                "_sp" | "r13" => regs.regs[13],
                "r14" => regs.regs[14],
                "_pc" | "r15" => regs.regs[15],
                "r16" => regs.regs[16],
                "r17" => regs.regs[17],
                _ => return Err(UDbgError::InvalidRegister),
            } as usize))
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn get_reg(&self, reg: &str, r: Option<&mut Registers>) -> Result<CpuReg, UDbgError> {
        let regs = self.regs();
        if let Some(r) = r {
            *r = unsafe { transmute(*regs) };
            Ok(CpuReg::Int(0))
        } else {
            Ok(CpuReg::Int(match reg {
                "pc" | "_pc" => regs.pc,
                "sp" | "_sp" => regs.sp,
                "pstate" => regs.pstate,
                "x0" => regs.regs[0],
                "x1" => regs.regs[1],
                "x2" => regs.regs[2],
                "x3" => regs.regs[3],
                "x4" => regs.regs[4],
                "x5" => regs.regs[5],
                "x6" => regs.regs[6],
                "x7" => regs.regs[7],
                "x8" => regs.regs[8],
                "x9" => regs.regs[9],
                "x10" => regs.regs[10],
                "x11" => regs.regs[11],
                "x12" => regs.regs[12],
                "x13" => regs.regs[13],
                "x14" => regs.regs[14],
                "x15" => regs.regs[15],
                "x16" => regs.regs[16],
                "x17" => regs.regs[17],
                "x18" => regs.regs[18],
                "x19" => regs.regs[19],
                "x20" => regs.regs[20],
                "x21" => regs.regs[21],
                "x22" => regs.regs[22],
                "x23" => regs.regs[23],
                "x24" => regs.regs[24],
                "x25" => regs.regs[25],
                "x26" => regs.regs[26],
                "x27" => regs.regs[27],
                "x28" => regs.regs[28],
                "x29" => regs.regs[29],
                "x30" => regs.regs[30],
                _ => return Err(UDbgError::InvalidRegister),
            } as usize))
        }
    }
}

fn trim_ver(name: &str) -> &str {
    use regex::Regex;
    &name[..Regex::new(r"-\d")
        .unwrap()
        .find(name)
        .map(|p| p.start())
        .unwrap_or(name.len())]
}

#[inline]
fn trim_allext(name: &str) -> &str {
    &name[..name.find(|c| c == '.').unwrap_or(name.len())]
}

#[inline]
fn trim_lastext(name: &str) -> &str {
    &name[..name.rfind(|c| c == '.').unwrap_or(name.len())]
}

pub fn ptrace_interrupt(tid: tid_t) -> bool {
    unsafe { ptrace(PTRACE_INTERRUPT as _, tid, 0, 0) == 0 }
}

pub fn ptrace_seize(tid: tid_t, flags: c_int) -> bool {
    unsafe { ptrace(PTRACE_SEIZE as _, tid, 0, flags) == 0 }
}

pub fn ptrace_getevtmsg<T: Copy>(tid: tid_t, result: &mut T) -> bool {
    unsafe { ptrace(PTRACE_GETEVENTMSG, tid, 0, result) == 0 }
}

pub fn ptrace_step_and_wait(tid: pid_t) -> bool {
    let tid = Pid::from_raw(tid);
    ptrace::step(tid, None);
    match waitpid(tid, None) {
        Ok(t) => {
            let pid = t.pid();
            if pid == Some(tid) {
                return true;
            }
            udbg_ui().error(format!("step unexpect tid: {pid:?}"));
            false
        }
        Err(_) => false,
    }
}

#[derive(Deref)]
pub struct StandardAdaptor(CommonAdaptor);

unsafe impl Send for StandardAdaptor {}
unsafe impl Sync for StandardAdaptor {}

impl StandardAdaptor {
    pub fn create(path: &str, args: &[&str]) -> UDbgResult<Arc<Self>> {
        use std::ffi::CString;
        unsafe {
            match libc::fork() {
                0 => {
                    ptrace(PTRACE_TRACEME, 0, 0, 0);
                    let path = CString::new(path).unwrap();
                    let args = args
                        .iter()
                        .map(|&arg| CString::new(arg).unwrap())
                        .collect::<Vec<_>>();
                    let mut argv = args.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
                    argv.insert(0, path.as_ptr());
                    argv.push(core::ptr::null());
                    execvp(path.as_ptr() as *const c_char, argv.as_ptr());
                    unreachable!();
                }
                -1 => Err(UDbgError::system()),
                pid => {
                    let ps = Process::from_pid(pid).ok_or_else(|| UDbgError::system())?;
                    let this = Self(CommonAdaptor::new(ps));
                    this.insert_thread(pid);
                    Ok(Arc::new(this))
                }
            }
        }
    }

    pub fn open(pid: pid_t) -> Result<Arc<Self>, UDbgError> {
        let ps = Process::from_pid(pid).ok_or(UDbgError::system())?;
        Ok(Arc::new(Self(CommonAdaptor::new(ps))))
    }

    pub fn insert_thread(&self, tid: tid_t) {
        if self.threads.write().insert(tid) {
            if let Err(err) = ptrace::setoptions(Pid::from_raw(tid), self.trace_opts) {
                udbg_ui().error(format!("ptrace_setopt {tid} {err:?}",));
            }
        }
    }

    pub fn remove_thread(&self, tid: tid_t, s: i32, tb: &mut TraceBuf) -> bool {
        let mut threads = self.threads.write();
        if threads.remove(&tid) {
            tb.call(UEvent::ThreadExit(s as u32));
            if threads.is_empty() {
                tb.call(UEvent::ProcessExit(s as u32));
                true
            } else {
                false
            }
        } else {
            udbg_ui().error(&format!("tid {tid} not found"));
            true
        }
    }
}

impl ReadMemory for StandardAdaptor {
    fn read_memory<'a>(&self, addr: usize, data: &'a mut [u8]) -> Option<&'a mut [u8]> {
        self.ps.read(addr, data)
    }
}

impl WriteMemory for StandardAdaptor {
    fn write_memory(&self, addr: usize, data: &[u8]) -> Option<usize> {
        self.ps.write(addr, data)
    }
}

impl TargetMemory for StandardAdaptor {
    fn enum_memory<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item = MemoryPage> + 'a>> {
        self.0.enum_memory()
    }

    fn virtual_query(&self, address: usize) -> Option<MemoryPage> {
        self.update_memory_page_check_time();
        RangeValue::binary_search(&self.mem_pages.read().as_slice(), address).map(|r| r.clone())
    }

    fn collect_memory_info(&self) -> Vec<MemoryPageInfo> {
        self.0.get_memory_map()
    }
}

impl GetProp for StandardAdaptor {
    fn get_prop(&self, key: &str) -> UDbgResult<serde_value::Value> {
        // match key {
        //     "moduleTimeout" => { self.tc_module.duration.set(Duration::from_secs_f64(s.args(3))); }
        //     "memoryTimeout" => { self.tc_memory.duration.set(Duration::from_secs_f64(s.args(3))); }
        //     _ => {}
        // }
        Ok(Value::Unit)
    }
}

impl TargetControl for StandardAdaptor {
    fn detach(&self) -> UDbgResult<()> {
        if self.base.is_opened() {
            self.base.status.set(UDbgStatus::Ended);
            return Ok(());
        }
        self.detaching.set(true);
        if self.waiting.get() {
            self.breakk()
        } else {
            // self.base.reply(UserReply::Run);
            Ok(())
        }
    }

    fn kill(&self) -> UDbgResult<()> {
        if unsafe { kill(self.ps.pid, SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(UDbgError::system())
        }
    }

    fn breakk(&self) -> UDbgResult<()> {
        self.base.check_opened()?;
        // for tid in self.enum_thread()? {
        //     if ptrace_interrupt(tid) {
        //         return Ok(());
        //     } else {
        //         println!("ptrace_interrupt({tid}) failed");
        //     }
        // }
        // return Err(UDbgError::system());
        match unsafe { kill(self.ps.pid, SIGSTOP) } {
            0 => Ok(()),
            code => Err(UDbgError::system()),
        }
    }
}

// impl TargetSymbol for StandardAdaptor {
// }

impl BreakpointManager for StandardAdaptor {
    fn add_bp(&self, opt: BpOpt) -> UDbgResult<Arc<dyn UDbgBreakpoint>> {
        self.base.check_opened()?;
        if self.bp_exists(opt.address as BpID) {
            return Err(UDbgError::BpExists);
        }

        let enable = opt.enable;
        let result = if let Some(rw) = opt.rw {
            return Err(UDbgError::NotSupport);
        } else {
            if let Some(origin) = self.readv::<BpInsn>(opt.address) {
                let bp = Breakpoint {
                    address: opt.address,
                    enabled: Cell::new(false),
                    temp: Cell::new(opt.temp),
                    hit_tid: opt.tid,
                    hit_count: Cell::new(0),
                    bp_type: InnerBpType::Soft(origin),

                    target: unsafe { Utils::to_weak(self) },
                    common: &self.0,
                };
                let bp = Arc::new(bp);
                self.bp_map.write().insert(bp.get_id(), bp.clone());
                Ok(bp)
            } else {
                Err(UDbgError::InvalidAddress)
            }
        };

        Ok(result.map(|bp| {
            if enable {
                bp.enable(true);
            }
            bp
        })?)
    }

    fn get_bp<'a>(&'a self, id: BpID) -> Option<Arc<dyn UDbgBreakpoint + 'a>> {
        Some(self.bp_map.read().get(&id)?.clone())
    }

    fn get_bp_list(&self) -> Vec<BpID> {
        self.bp_map.read().keys().cloned().collect()
    }
}

impl Target for StandardAdaptor {
    fn base(&self) -> &TargetBase {
        &self.base
    }

    fn enum_module<'a>(
        &'a self,
    ) -> UDbgResult<Box<dyn Iterator<Item = Arc<dyn UDbgModule + 'a>> + 'a>> {
        self.0.enum_module()
    }

    fn find_module(&self, module: usize) -> Option<Arc<dyn UDbgModule>> {
        let mut result = self.symgr.find_module(module);
        self.tc_module.check(|| {
            self.update_module();
            result = self.symgr.find_module(module);
        });
        Some(result?)
    }

    fn get_module(&self, module: &str) -> Option<Arc<dyn UDbgModule>> {
        Some(self.symgr.get_module(module).or_else(|| {
            self.0.update_module();
            self.symgr.get_module(module)
        })?)
    }

    fn open_thread(&self, tid: tid_t) -> UDbgResult<Box<dyn UDbgThread>> {
        let task = Task::new(self.ps.pid, tid).context("task")?;
        Ok(Box::new(NixThread {
            base: ThreadData { tid, wow64: false },
            stat: task.stat().context("stat")?,
        }))
    }

    fn enum_handle<'a>(&'a self) -> UDbgResult<Box<dyn Iterator<Item = HandleInfo> + 'a>> {
        self.0.enum_handle()
    }

    fn enum_thread(
        &self,
        detail: bool,
    ) -> UDbgResult<Box<dyn Iterator<Item = Box<dyn UDbgThread>> + '_>> {
        Ok(Box::new(
            self.ps
                .enum_thread()
                .filter_map(|tid| self.open_thread(tid).ok()),
        ))
    }
}

impl UDbgAdaptor for StandardAdaptor {
    fn get_registers<'a>(&'a self) -> UDbgResult<&'a mut dyn UDbgRegs> {
        Ok(self.regs() as &mut dyn UDbgRegs)
    }
}

pub struct TraceBuf<'a> {
    pub callback: &'a mut UDbgCallback<'a>,
    pub target: Arc<StandardAdaptor>,
}

impl TraceBuf<'_> {
    #[inline]
    pub fn call(&mut self, event: UEvent) -> UserReply {
        (self.callback)(self.target.clone(), event)
    }
}

pub type HandleResult = Option<Signal>;

pub trait EventHandler {
    /// fetch a debug event
    fn fetch(&mut self, buf: &mut TraceBuf) -> Option<()>;
    /// handle the debug event
    fn handle(&mut self, buf: &mut TraceBuf) -> Option<HandleResult>;
    /// continue debug event
    fn cont(&mut self, _: HandleResult, buf: &mut TraceBuf);
}

pub struct DefaultEngine {
    targets: Vec<Arc<StandardAdaptor>>,
    status: WaitStatus,
    inited: bool,
    cloned_tids: HashSet<tid_t>,
    tid: tid_t,
}

impl Default for DefaultEngine {
    fn default() -> Self {
        Self {
            targets: Default::default(),
            status: WaitStatus::StillAlive,
            inited: false,
            tid: 0,
            cloned_tids: Default::default(),
        }
    }
}

impl EventHandler for DefaultEngine {
    fn fetch(&mut self, buf: &mut TraceBuf) -> Option<()> {
        self.status = waitpid(None, Some(WaitPidFlag::__WALL | WaitPidFlag::WUNTRACED)).ok()?;
        info!("[status] {:?}", self.status);

        self.tid = self.status.pid().map(|p| p.as_raw()).unwrap_or_default();

        let target = self
            .targets
            .iter()
            .find(|&t| t.threads.read().contains(&self.tid))
            .cloned()
            .or_else(|| {
                self.targets
                    .iter()
                    .find(|&t| Task::new(t.ps.pid, self.tid).is_ok())
                    .cloned()
            })
            .expect("not traced target");

        buf.target.base.event_tid.set(self.tid);
        Some(())
    }

    fn handle(&mut self, buf: &mut TraceBuf) -> Option<HandleResult> {
        let status = self.status.clone();
        let this = buf.target.clone();
        let tid = self.tid;

        Some(match status {
            WaitStatus::Stopped(_, sig) => loop {
                this.update_regs(tid);
                let regs = unsafe { &mut *this.regs.get() };
                if !self.inited && matches!(sig, Signal::SIGSTOP | Signal::SIGTRAP) {
                    self.inited = true;
                    buf.call(UEvent::InitBp);
                    this.insert_thread(tid);
                    break None;
                }
                match sig {
                    // maybe thread created (by ptrace_attach or ptrace_interrupt) (in PTRACE_EVENT_CLONE)
                    // maybe kill by SIGSTOP
                    Signal::SIGSTOP => {
                        if this.threads.read().get(&tid).is_none() {
                            this.insert_thread(tid);
                            break None;
                        }
                        if self.cloned_tids.remove(&tid) {
                            break None;
                        }
                    }
                    Signal::SIGTRAP | Signal::SIGILL => {
                        let si =
                            ::nix::sys::ptrace::getsiginfo(Pid::from_raw(tid)).expect("siginfo");
                        // let info = this.ps.siginfo(tid).expect("siginfo");
                        println!("stop info: {si:?}, pc: {:p}", unsafe { si.si_addr() });
                        // match info.si_code {
                        //     TRAP_BRKPT => println!("info.si_code TRAP_BRKPT"),
                        //     TRAP_HWBKPT => println!("info.si_code TRAP_HWBKPT"),
                        //     TRAP_TRACE => println!("info.si_code TRAP_TRACE"),
                        //     code => println!("info.si_code {}", code),
                        // };
                        let ip = *regs.ip();
                        let address = if sig == Signal::SIGTRAP && ip > 0 {
                            ip - 1
                        } else {
                            ip
                        };
                        *regs.ip() = address;
                        // println!("sigtrap address {:x}", address);
                        if let Some(bp) = this.get_bp_(address as BpID) {
                            this.handle_breakpoint(this.as_ref(), tid, &si, bp, buf);
                            break None;
                        }
                    }
                    _ => {}
                }
                buf.call(UEvent::Exception {
                    first: true,
                    code: sig as _,
                });
                break Some(sig);
            },
            WaitStatus::PtraceEvent(_, sig, code) => {
                match code {
                    PTRACE_EVENT_STOP => {
                        this.insert_thread(tid);
                    }
                    PTRACE_EVENT_CLONE => {
                        let mut new_tid: tid_t = 0;
                        ptrace_getevtmsg(tid, &mut new_tid);
                        buf.call(UEvent::ThreadCreate(new_tid));
                        // trace new thread
                        ptrace::attach(Pid::from_raw(new_tid));
                        // set trace options for new thread
                        this.insert_thread(new_tid);

                        self.cloned_tids.insert(new_tid);
                    }
                    PTRACE_EVENT_FORK => {}
                    _ => {}
                }
                None
            }
            // exited with exception
            WaitStatus::Signaled(_, sig, coredump) => {
                buf.call(UEvent::Exception {
                    first: false,
                    code: sig as _,
                });
                if !matches!(sig, Signal::SIGSTOP) {
                    this.remove_thread(tid, -1, buf);
                }
                Some(sig)
            }
            // exited normally
            WaitStatus::Exited(_, code) => {
                this.remove_thread(tid, code, buf);
                None
            }
            _ => unreachable!("status: {status:?}"),
        })
    }

    fn cont(&mut self, sig: HandleResult, buf: &mut TraceBuf) {
        let this = buf.target.clone();
        if this.detaching.get() {
            for bp in this.get_breakpoints() {
                bp.remove();
            }
            for &tid in this.threads.read().iter() {
                if let Err(err) = ptrace::detach(Pid::from_raw(tid), None) {
                    udbg_ui().error(format!("ptrace_detach({tid}) failed: {err:?}"));
                }
            }
        }
        ptrace::cont(Pid::from_raw(self.tid), sig);
    }
}

impl UDbgEngine for DefaultEngine {
    fn open(&mut self, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        Ok(StandardAdaptor::open(pid)?)
    }

    fn attach(&mut self, pid: pid_t) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        let this = StandardAdaptor::open(pid)?;
        // attach each of threads
        for tid in this.0.ps.enum_thread() {
            ptrace::attach(Pid::from_raw(tid)).context("attach")?;
            // this.threads.write().insert(tid);
            this.insert_thread(tid);
        }
        self.targets.push(this.clone());
        Ok(this)
    }

    fn create(
        &mut self,
        path: &str,
        cwd: Option<&str>,
        args: &[&str],
    ) -> UDbgResult<Arc<dyn UDbgAdaptor>> {
        let result = StandardAdaptor::create(path, args)?;
        self.targets.push(result.clone());
        Ok(result)
    }

    fn event_loop<'a>(&mut self, callback: &mut UDbgCallback<'a>) -> UDbgResult<()> {
        self.targets.iter().for_each(|t| {
            t.update_module();
            t.update_memory_page();
        });

        // if self.base.is_opened() {
        //     use std::time::Duration;
        //     while self.base.status.get() != UDbgStatus::Ended {
        //         std::thread::sleep(Duration::from_millis(10));
        //     }
        //     return Ok(());
        // }
        let mut buf = TraceBuf {
            callback,
            target: self
                .targets
                .iter()
                .next()
                .map(Clone::clone)
                .expect("no attached target"),
        };

        while let Some(s) = self.fetch(&mut buf).and_then(|_| self.handle(&mut buf)) {
            self.cont(s, &mut buf);
        }

        Ok(())
    }
}