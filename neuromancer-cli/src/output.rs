pub fn print_success(json_mode: bool, payload: &serde_json::Value) {
    if json_mode {
        let envelope = serde_json::json!({
            "ok": true,
            "result": payload,
        });
        println!("{envelope}");
        return;
    }

    if let Some(message) = payload.as_str() {
        println!("{message}");
        return;
    }

    match serde_json::to_string_pretty(payload) {
        Ok(rendered) => println!("{rendered}"),
        Err(_) => println!("{payload}"),
    }
}

pub fn print_error(json_mode: bool, message: &str, exit_code: i32) {
    if json_mode {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "message": message,
                "code": exit_code,
            }
        });
        eprintln!("{envelope}");
        return;
    }

    eprintln!("error: {message}");
}
