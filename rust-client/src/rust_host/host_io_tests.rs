use super::{host_io_bindings as host, scheduler, world};
use mlua::{Lua, MultiValue};
use serde_json::json;
use std::{
    cell::RefCell,
    io::{Read, Write},
    net::TcpListener,
    rc::Rc,
    time::{Duration, Instant},
};

fn fixture() -> (Lua, world::WorldRef, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(
            &json!({
                "version":1,"sequence":0,"nodes":[{"id":"game","class_name":"DataModel",
                "name":"game","parent":null,"properties":{}}]
            }),
            None,
        )
        .unwrap(),
    ));
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    super::install_bindings(&lua, &world).unwrap();
    (lua, world, scheduler)
}

fn drive(lua: &Lua, scheduler: &scheduler::SchedulerRef, thread: &mlua::Thread) -> MultiValue {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        host::pump(lua).unwrap();
        scheduler::step(lua, scheduler, Instant::now(), Duration::from_millis(10));
        match scheduler::take_outcome(scheduler, thread) {
            Some(scheduler::TaskOutcome::Completed(values)) => return values,
            Some(scheduler::TaskOutcome::Error(error)) => panic!("host call failed: {error}"),
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "local host operation did not finish"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn http_await_uses_local_worker_and_preserves_binary_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/test", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut input = Vec::new();
        while !input.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            input.push(byte[0]);
            assert!(input.len() < 16_384);
        }
        let request = String::from_utf8_lossy(&input).to_lowercase();
        assert!(!request.contains("\r\ncookie:"));
        assert!(!request.contains("\r\nauthorization:"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na\0b")
            .unwrap();
    });
    let (lua, _world, scheduler) = fixture();
    let function = lua
        .load("local url=...; local r=request({Url=url}); return r.StatusCode, r.Body")
        .into_function()
        .unwrap();
    let task = scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::from_vec(vec![mlua::Value::String(lua.create_string(url).unwrap())]),
        scheduler::TaskPolicy::default(),
    )
    .unwrap();
    let result = drive(&lua, &scheduler, &task);
    assert_eq!(result[0].as_i64(), Some(200));
    assert_eq!(result[1].as_string().unwrap().as_bytes().as_ref(), b"a\0b");
    host::close(&lua);
    server.join().unwrap();
}

#[test]
fn websocket_delivers_buffered_frames_and_binary_echo_then_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/echo", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        socket
            .send(tungstenite::Message::Text("ready".into()))
            .unwrap();
        let message = socket.read().unwrap();
        assert!(message.is_binary());
        socket.send(message).unwrap();
        assert!(socket.read().unwrap().is_close());
    });
    let (lua, _world, scheduler) = fixture();
    let function = lua
        .load(
            r#"
        local socket = WebSocket.connect(...)
        local messages, closes = {}, 0
        socket.OnMessage:Connect(function(data) table.insert(messages,data) end)
        socket.OnClose:Connect(function() closes += 1 end)
        socket:Send("a\0b", true)
        while #messages < 2 do task.wait(0.002) end
        socket:Close()
        task.wait(0)
        return messages[1], messages[2], closes
    "#,
        )
        .into_function()
        .unwrap();
    let task = scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::from_vec(vec![mlua::Value::String(lua.create_string(url).unwrap())]),
        scheduler::TaskPolicy::default(),
    )
    .unwrap();
    let result = drive(&lua, &scheduler, &task);
    assert_eq!(result[0].as_string().unwrap().to_str().unwrap(), "ready");
    assert_eq!(result[1].as_string().unwrap().as_bytes().as_ref(), b"a\0b");
    assert_eq!(result[2].as_i64(), Some(1));
    host::close(&lua);
    server.join().unwrap();
}

#[test]
fn console_input_resumes_only_the_pending_request_and_destroy_wakes_waiters() {
    let (lua, _world, scheduler) = fixture();
    let function = lua.load("return rconsoleinput()").into_function().unwrap();
    let task = scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::new(),
        scheduler::TaskPolicy::default(),
    )
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    let events = host::drain(&lua);
    let id = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "input")
        .unwrap()["id"]
        .as_u64()
        .unwrap();
    assert!(host::command(&lua, &json!({"action":"input","id":id+1,"text":"wrong"})).is_err());
    host::command(&lua, &json!({"action":"input","id":id,"text":"answer"})).unwrap();
    let result = drive(&lua, &scheduler, &task);
    assert_eq!(result[0].as_string().unwrap().to_str().unwrap(), "answer");
    let function = lua
        .load("return pcall(rconsoleinput)")
        .into_function()
        .unwrap();
    let task = scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::new(),
        scheduler::TaskPolicy::default(),
    )
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    lua.load("rconsoledestroy()").exec().unwrap();
    let result = drive(&lua, &scheduler, &task);
    assert_eq!(result[0].as_boolean(), Some(false));
    assert!(result[1]
        .as_string()
        .unwrap()
        .to_string_lossy()
        .contains("destroyed"));
    host::close(&lua);
}
