use std::{cell::RefCell, num::NonZero, sync::Arc};
use turso_core::{
    io::{clock::DefaultClock, FileId, FileSyncType}, Buffer, Clock, Completion, Connection,
    Database, File, IO, LimboError, MonotonicInstant, OpenFlags, Result, SqliteDialect,
    StepResult, Value, WallClockInstant,
};

#[link(wasm_import_module = "vfs")]
extern "C" {
    fn open(path: *const u8, len: usize, create: i32) -> i32;
    fn remove(path: *const u8, len: usize) -> i32;
    fn read(fd: i32, offset: f64, ptr: *mut u8, len: usize) -> i32;
    fn write(fd: i32, offset: f64, ptr: *const u8, len: usize) -> i32;
    fn length(fd: i32) -> f64;
    fn resize(fd: i32, len: f64) -> i32;
    fn error_text(ptr: *const u8, len: usize);
    fn result_int(value: i64);
}

struct Vfs;
struct VirtualFile(i32);

fn checked(n: i32) -> Result<i32> {
    if n < 0 { Err(LimboError::InternalError(format!("virtual file operation failed ({n})"))) }
    else { Ok(n) }
}

impl Clock for Vfs {
    fn current_time_monotonic(&self) -> MonotonicInstant { DefaultClock.current_time_monotonic() }
    fn current_time_wall_clock(&self) -> WallClockInstant { DefaultClock.current_time_wall_clock() }
}

impl IO for Vfs {
    fn open_file(&self, path: &str, flags: OpenFlags, _: bool) -> Result<Arc<dyn File>> {
        let fd = unsafe { open(path.as_ptr(), path.len(), flags.contains(OpenFlags::Create) as i32) };
        if fd < 0 {
            return Err(turso_core::error::CompletionError::IOError(std::io::ErrorKind::NotFound, "open").into());
        }
        Ok(Arc::new(VirtualFile(fd)))
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        checked(unsafe { remove(path.as_ptr(), path.len()) })?;
        Ok(())
    }
    fn file_id(&self, path: &str) -> Result<FileId> { Ok(FileId::from_path_hash(path)) }
}

impl File for VirtualFile {
    fn lock_file(&self, _: bool) -> Result<()> { Ok(()) }
    fn unlock_file(&self) -> Result<()> { Ok(()) }
    fn pread(&self, offset: u64, c: Completion) -> Result<Completion> {
        let buf = c.as_read().buf();
        let n = checked(unsafe { read(self.0, offset as f64, buf.as_mut_ptr(), buf.len()) })?;
        c.complete(n);
        Ok(c)
    }
    fn pwrite(&self, offset: u64, buf: Arc<Buffer>, c: Completion) -> Result<Completion> {
        let n = checked(unsafe { write(self.0, offset as f64, buf.as_ptr(), buf.len()) })?;
        c.complete(n);
        Ok(c)
    }
    fn sync(&self, c: Completion, _: FileSyncType) -> Result<Completion> { c.complete(0); Ok(c) }
    fn size(&self) -> Result<u64> { Ok(unsafe { length(self.0) } as u64) }
    fn truncate(&self, len: u64, c: Completion) -> Result<Completion> {
        checked(unsafe { resize(self.0, len as f64) })?;
        c.complete(0);
        Ok(c)
    }
}

thread_local! { static CONNECTION: RefCell<Option<Arc<Connection>>> = const { RefCell::new(None) }; }

fn report(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            let message = e.to_string();
            unsafe { error_text(message.as_ptr(), message.len()); }
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn allocate(len: usize) -> *mut u8 { Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8 }

#[no_mangle]
pub unsafe extern "C" fn release(ptr: *mut u8, len: usize) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}

#[no_mangle]
pub extern "C" fn open_engine() -> i32 {
    report((|| {
        let db = Database::open_file(Arc::new(Vfs), "merchant.db", Arc::new(SqliteDialect))?;
        let conn = db.connect()?;
        CONNECTION.with(|slot| *slot.borrow_mut() = Some(conn));
        Ok(())
    })())
}

// The merchant API uses integer keys and returns at most one integer column per row.
#[no_mangle]
pub unsafe extern "C" fn execute(ptr: *const u8, len: usize, a: i64, b: i64, count: u32) -> i32 {
    report((|| {
        let sql = std::str::from_utf8(std::slice::from_raw_parts(ptr, len))
            .map_err(|e| LimboError::InternalError(e.to_string()))?;
        let conn = CONNECTION.with(|slot| slot.borrow().clone())
            .ok_or_else(|| LimboError::InternalError("database is closed".into()))?;
        let mut stmt = conn.prepare(sql)?;
        for (i, value) in [a,b].into_iter().take(count as usize).enumerate() {
            stmt.bind_at(NonZero::new(i+1).unwrap(), Value::from(value))?;
        }
        loop {
            match stmt.step()? {
                StepResult::Row => result_int(stmt.row().unwrap().get::<i64>(0)?),
                StepResult::Done => break,
                StepResult::IO | StepResult::Yield => continue,
                _ => return Err(LimboError::InternalError("statement was busy or interrupted".into())),
            }
        }
        Ok(())
    })())
}
