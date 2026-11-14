use super::*;

#[test]
fn load_source_reports_real_compile_storage_copy_and_execution() -> mlua::Result<()> {
    let lua = Lua::new();
    install(&lua)?;

    let function = load_source(&lua, "=integrity-test", "return 40 + 2")?;
    assert_eq!(function.call::<i64>(())?, 42);

    let (thread, protected) = report(&lua)?;
    assert_eq!(thread["checks"], 1);
    assert_eq!(thread["failures"], 0);
    assert_eq!(protected["constructions"], 1);
    assert_eq!(protected["copies"], 1);
    assert_eq!(protected["failures"], 0);
    Ok(())
}

#[test]
fn compile_failure_counts_the_thread_check_but_not_a_payload() -> mlua::Result<()> {
    let lua = Lua::new();
    install(&lua)?;

    assert!(load_source(&lua, "=bad-source", "local =").is_err());
    let (thread, protected) = report(&lua)?;
    assert_eq!(thread["checks"], 1);
    assert_eq!(protected["constructions"], 0);
    assert_eq!(protected["copies"], 0);
    Ok(())
}

#[test]
fn non_owner_thread_check_is_counted_as_a_failure() {
    let mut observer = IntegrityState::new();
    let other_thread = thread::spawn(|| thread::current().id()).join().unwrap();
    assert!(observer.check_thread(other_thread).is_err());
    assert_eq!(observer.counters.thread_checks, 1);
    assert_eq!(observer.counters.thread_failures, 1);
}

#[test]
fn interned_source_and_bytecode_share_real_arc_identity() -> mlua::Result<()> {
    let state = Rc::new(std::cell::RefCell::new(IntegrityState::new()));
    let first = ProtectedSource::new(state.clone(), b"source", b"compiled")?;
    let second = ProtectedSource::new(state.clone(), b"source", b"compiled")?;
    assert!(Arc::ptr_eq(&first.source, &second.source));
    assert!(Arc::ptr_eq(&first.bytecode, &second.bytecode));
    let copy = first.checked_clone()?;
    assert!(Arc::ptr_eq(&first.source, &copy.source));
    assert_eq!(state.borrow().counters.constructions, 2);
    assert_eq!(state.borrow().counters.copies, 1);
    assert_eq!(state.borrow().counters.storage_failures, 0);
    Ok(())
}
