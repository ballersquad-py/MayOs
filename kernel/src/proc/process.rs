pub struct Process { pub pid: u64, pub name: alloc::string::String, pub pml4: u64 }
pub fn exit_current_process(_code: i64, _msg: Option<&str>) {}
