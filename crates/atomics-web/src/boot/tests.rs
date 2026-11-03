use super::*;
use atomics_core::noise::Secret;

const PARENT: &str = "https://app.example";
const HOST: &str = "6f1c2a7e-0b7d-4c55-9f43-3d2a1b0c9e8f";

fn parent() -> ParentOrigin {
    ParentOrigin::parse(PARENT).expect("an origin")
}

fn boot_message() -> Value {
    json!({
        "type": BOOT, "v": 1, "host_id": HOST, "ticket": "t0K-en.x_1", "broker": "wss://app.example/atomics/tunnel",
        "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
    })
}

fn keys() -> (String, String) {
    let host = Secret::generate().unwrap().public().text();
    let psk = Pairing::new(HOST, Secret::generate().unwrap().public()).unwrap();
    let psk = psk.fragment().rsplit('.').next().unwrap().to_string();
    (host, psk)
}

#[test]
fn the_parent_origin_is_exactly_an_origin() {
    assert_eq!(parent().as_str(), PARENT);
    assert_eq!(ParentOrigin::parse(" HTTPS://App.Example/ ").unwrap().as_str(), PARENT);
    assert_eq!(ParentOrigin::parse("https://app.example:443").unwrap().as_str(), PARENT);
    assert_eq!(ParentOrigin::parse("https://app.example:8443").unwrap().as_str(), "https://app.example:8443");
    assert_eq!(ParentOrigin::parse("http://127.0.0.1:5173").unwrap().as_str(), "http://127.0.0.1:5173");
    assert_eq!(ParentOrigin::parse("http://[::1]:5173").unwrap().as_str(), "http://[::1]:5173");
    for refused in [
        "",
        "app.example",
        "https://",
        "https://app.example/atomics",
        "https://app.example?x=1",
        "https://app.example#x",
        "https://user@app.example",
        "http://app.example",
        "javascript://app",
        "https://app.example:0",
        "https://app.example:99999",
        "https://app.example:",
        "https://*.example",
        "https://app .example",
        "https://-app.example",
        "null",
        "*",
    ] {
        assert_eq!(ParentOrigin::parse(refused), None, "{refused:?}");
    }
}

#[test]
fn only_the_exact_origin_and_the_parent_window_are_heard() {
    let message = boot_message();
    assert!(matches!(read(&parent(), PARENT, true, &message), Ok(FromParent::Boot(_))));
    for other in [
        "https://app.example:443",
        "https://app.example.evil.test",
        "https://evil.app.example",
        "http://app.example",
        "HTTPS://APP.EXAMPLE",
        "null",
        "",
    ] {
        assert_eq!(read(&parent(), other, true, &message).unwrap_err(), Refused::Origin, "{other:?}");
    }
    assert_eq!(read(&parent(), PARENT, false, &message).unwrap_err(), Refused::Window, "another window of that origin");
}

#[test]
fn a_boot_carries_the_host_the_ticket_the_broker_and_the_look() {
    let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &boot_message()) else { panic!("a boot") };
    assert_eq!(boot.host_id, HOST);
    assert_eq!(boot.broker, "wss://app.example/atomics/tunnel");
    assert_eq!((boot.display_name.as_str(), boot.theme.as_str(), boot.locale.as_str()), ("A Person", "dark", "en"));
    assert_eq!((boot.scheme, boot.view.as_deref()), (Scheme::Dark, Some("board")));
    assert_eq!(boot.ticket_key, None, "no ticket asked for");
    assert!(boot.pairing.is_none());
    assert_eq!(boot.ticket.subprotocol().as_str(), "atomics.ticket.t0K-en.x_1");
    let shown = format!("{boot:?} {:?}", boot.ticket);
    assert!(!shown.contains("t0K-en"), "Debug never shows the ticket: {shown}");
    // A light page gets a light ATOMICS theme when it names none of ATOMICS's.
    let mut light = boot_message();
    light["scheme"] = json!("light");
    light["theme"] = json!("eclipse");
    let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &light) else { panic!("a boot") };
    assert_eq!(boot.theme_tokens().mode, Some(ThemeMode::Light));
    light["theme"] = json!("midnight");
    let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &light) else { panic!("a boot") };
    assert_eq!(boot.theme_tokens().mode, Some(ThemeMode::Dark), "a built-in theme by name wins over the scheme");
}

#[test]
fn malformed_boots_are_refused_by_field() {
    let cases: &[(&str, Value, Refused)] = &[
        ("type", json!("other.atomics.boot"), Refused::Unknown),
        ("v", json!(2), Refused::Unknown),
        ("v", json!("1"), Refused::Unknown),
        ("host_id", json!("a/b"), Refused::Field("host_id")),
        ("host_id", json!(""), Refused::Field("host_id")),
        ("ticket", json!(""), Refused::Field("ticket")),
        ("ticket", json!("has space"), Refused::Field("ticket")),
        ("ticket", json!("a,b"), Refused::Field("ticket")),
        ("ticket", json!("x".repeat(MAX_TICKET_BYTES + 1)), Refused::Field("ticket")),
        ("ticket", json!(7), Refused::Field("ticket")),
        ("broker", json!("https://app.example/atomics/tunnel"), Refused::Field("broker")),
        ("broker", json!("ws://app.example/atomics/tunnel"), Refused::Field("broker")),
        ("broker", json!("wss://user@app.example/t"), Refused::Field("broker")),
        ("broker", json!("wss://app.example/t#x"), Refused::Field("broker")),
        ("broker", json!("/atomics/tunnel"), Refused::Field("broker")),
        ("mode", json!("other"), Refused::Field("mode")),
        ("mode", json!("pair"), Refused::Field("pair")),
    ];
    for (field, value, refused) in cases {
        let mut message = boot_message();
        message[*field] = value.clone();
        assert_eq!(read(&parent(), PARENT, true, &message).unwrap_err(), *refused, "{field} = {value}");
    }
    for missing in ["host_id", "ticket", "broker"] {
        let mut message = boot_message();
        message.as_object_mut().unwrap().remove(missing);
        assert_eq!(read(&parent(), PARENT, true, &message).unwrap_err(), Refused::Field(missing));
    }
    for other in [json!(null), json!("atomics.embed.boot"), json!([BOOT]), json!({ "type": 1, "v": 1 })] {
        assert_eq!(read(&parent(), PARENT, true, &other).unwrap_err(), Refused::Unknown, "{other}");
    }
    // A loopback broker is a local preview; an odd view is dropped, not refused.
    let mut local = boot_message();
    local["broker"] = json!("ws://127.0.0.1:5173/atomics/tunnel");
    local["view"] = json!("no spaces!");
    local["display_name"] = json!("  A\u{7}Person  ");
    let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &local) else { panic!("a boot") };
    assert_eq!((boot.view, boot.display_name.as_str()), (None, "APerson"));
    // A ticket key is a text like the others; an empty or odd one opens nothing in particular.
    for (value, key) in [
        (json!(" ENG-40\u{1b} "), Some("ENG-40")),
        (json!("  "), None),
        (json!(42), None),
        (json!("k".repeat(300)), Some(&*"k".repeat(200))),
    ] {
        let mut message = boot_message();
        message["ticket_key"] = value.clone();
        let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &message) else { panic!("{value}") };
        assert_eq!(boot.ticket_key.as_deref(), key, "{value}");
    }
}

#[test]
fn a_pairing_boot_reads_the_links_keys() {
    let (host_key, psk) = keys();
    let mut message = boot_message();
    message["mode"] = json!("pair");
    message["pair"] = json!({ "v": 1, "host_noise_pubkey": host_key, "psk": psk });
    let Ok(FromParent::Boot(boot)) = read(&parent(), PARENT, true, &message) else { panic!("a boot") };
    let pairing = boot.pairing.as_ref().expect("a pairing");
    assert_eq!((pairing.host_id.as_str(), pairing.host_key.text()), (HOST, host_key.clone()));
    assert!(!format!("{boot:?}").contains(&psk), "Debug never shows the psk");

    for (field, value) in [
        ("v", json!(2)),
        ("v", json!("1")),
        ("host_noise_pubkey", json!("short")),
        ("psk", json!(format!("{}=", &psk[..42]))),
        ("psk", json!(null)),
    ] {
        let mut bad = message.clone();
        bad["pair"][field] = value.clone();
        assert!(
            matches!(read(&parent(), PARENT, true, &bad), Err(Refused::Field(_))),
            "pair.{field} = {value} is refused"
        );
    }
}

#[test]
fn a_fresh_ticket_names_its_host() {
    let message = json!({ "type": TICKET, "v": 1, "host_id": HOST, "ticket": "fresh" });
    let Ok(FromParent::Ticket { host_id, ticket }) = read(&parent(), PARENT, true, &message) else {
        panic!("a ticket")
    };
    assert_eq!((host_id.as_str(), ticket.subprotocol().as_str()), (HOST, "atomics.ticket.fresh"));
    let mut bad = message.clone();
    bad["ticket"] = json!("");
    assert_eq!(read(&parent(), PARENT, true, &bad).unwrap_err(), Refused::Field("ticket"));
    assert_eq!(read(&parent(), "https://evil.test", true, &message).unwrap_err(), Refused::Origin);
}

#[test]
fn the_frame_says_four_things() {
    for kind in [READY, BOOTED, WANTS_TICKET, PAIRED] {
        assert_eq!(say(kind), json!({ "type": kind, "v": 1 }));
    }
}

#[test]
fn brokers_are_secure_or_on_this_machine() {
    for ok in ["wss://b.example/atomics/tunnel", "wss://b.example:8443/t?x=1", "ws://localhost:8080/t", "ws://[::1]/t"]
    {
        assert!(broker_ok(ok), "{ok}");
    }
    for bad in ["ws://b.example/t", "http://b.example", "wss://", "wss://b .example/t", "wss://b.example/t\n", "wss:/b"]
    {
        assert!(!broker_ok(bad), "{bad}");
    }
}
