use std::time::{Duration, Instant};

use clap::Parser;
use rustbus::connection::{Timeout, ll_conn::DuplexConn};
use rustbus::message_builder::{MarshalledMessage, MessageBuilder, MessageType};
use rustbus::params::{Base, Container, DictMap, Param};

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const MENU_IFACE: &str = "com.canonical.dbusmenu";

#[derive(Parser)]
#[command(name = "tray-inspector", about = "Inspect system tray items and their menus")]
struct Args {
    #[arg(short, long, help = "Show full properties")]
    verbose: bool,

    #[arg(long, help = "Show menu tree")]
    menu: bool,

    #[arg(long, help = "Filter by item title")]
    name: Option<String>,

    #[arg(long, help = "Filter by item ID")]
    id: Option<String>,
}

fn main() {
    let args = Args::parse();

    let mut conn = match connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to connect to D-Bus session bus: {e}");
            std::process::exit(1);
        }
    };

    let addresses = match get_registered_items(&mut conn) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: failed to query StatusNotifierWatcher: {e}");
            std::process::exit(1);
        }
    };

    if addresses.is_empty() {
        println!("No system tray items found.");
        return;
    }

    let items = collect_items(&mut conn, &addresses, &args);

    if items.is_empty() {
        println!("No matching system tray items found.");
        return;
    }

    let brief = !args.verbose && !args.menu && args.name.is_none() && args.id.is_none();

    if brief {
        print_items_brief(&items);
    } else {
        for (bus_name, object_path, item_props) in &items {
            let lines = build_item_lines(bus_name, object_path, item_props);
            let max_key = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            let max_val = lines.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
            let width = max_key + max_val + 6;

            let fmt_line = |key: &str, val: &str| format!("│ {key:>max_key$}: {val}");
            let top = format!("┌─ Item {}", "─".repeat(width - 8));
            let sep = format!("├─ Menu {}", "─".repeat(width - 8));
            let bot = format!("└{}", "─".repeat(width - 1));

            println!("{top}");
            for (key, val) in &lines {
                println!("{}", fmt_line(key, val));
            }

            if args.menu {
                if let Some(menu_path) = item_props.props.get("Menu") {
                    if !menu_path.is_empty() && menu_path != "-" && menu_path != "/" {
                        match get_menu_layout(&mut conn, bus_name, menu_path) {
                            Ok(nodes) => {
                                println!("{sep}");
                                print_menu_nodes(&nodes, width, &fmt_line);
                            }
                            Err(e) => println!("│ [{e}]"),
                        }
                    }
                }
            }

            println!("{bot}");
            println!();
        }
    }
}

fn connect() -> Result<DuplexConn, String> {
    let path = rustbus::get_session_bus_path().map_err(|e| e.to_string())?;
    let mut conn = DuplexConn::connect_to_bus(path, false).map_err(|e| e.to_string())?;
    conn.send_hello(Timeout::Infinite).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn do_call(conn: &mut DuplexConn, msg: MarshalledMessage) -> Result<MarshalledMessage, String> {
    let serial = conn.send.send_message_write_all(&msg).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("D-Bus call timed out".into());
        }
        let resp = conn
            .recv
            .get_next_message(Timeout::Duration(remaining))
            .map_err(|e| e.to_string())?;
        if resp.dynheader.response_serial == Some(serial) {
            return match resp.typ {
                MessageType::Reply => Ok(resp),
                MessageType::Error => {
                    let name: String = resp.body.parser().get().unwrap_or_default();
                    Err(format!("D-Bus error: {name}"))
                }
                _ => continue,
            };
        }
    }
}

fn get_registered_items(conn: &mut DuplexConn) -> Result<Vec<(String, String)>, String> {
    let mut call = MessageBuilder::new()
        .call("Get")
        .on(WATCHER_PATH)
        .with_interface(PROPS_IFACE)
        .at(WATCHER_BUS)
        .build();
    call.body.push_param(WATCHER_IFACE).map_err(|e| e.to_string())?;
    call.body.push_param("RegisteredStatusNotifierItems").map_err(|e| e.to_string())?;

    let reply = do_call(conn, call)?;

    let mut parser = reply.body.parser();
    let param = parser.get_param().map_err(|e| e.to_string())?;
    let inner = match &param {
        Param::Container(Container::Variant(v)) => &v.value,
        _ => return Err("expected variant from Properties.Get".into()),
    };
    let array = match inner {
        Param::Container(Container::Array(arr)) => arr,
        _ => return Err("expected array from RegisteredStatusNotifierItems".into()),
    };

    let mut result = Vec::new();
    for val in &array.values {
        let s = match val {
            Param::Base(Base::StringRef(s)) => s.to_string(),
            Param::Base(Base::String(s)) => s.clone(),
            _ => continue,
        };
        result.push(parse_address(&s));
    }
    Ok(result)
}

fn parse_address(address: &str) -> (String, String) {
    match address.split_once('/') {
        Some((bus, path)) => (bus.to_string(), format!("/{path}")),
        None => (address.to_string(), "/StatusNotifierItem".to_string()),
    }
}

struct ItemProps {
    props: std::collections::HashMap<String, String>,
    id: String,
    title: String,
}

fn get_item_properties(
    conn: &mut DuplexConn,
    bus_name: &str,
    object_path: &str,
) -> Result<ItemProps, String> {
    let mut call = MessageBuilder::new()
        .call("GetAll")
        .on(object_path)
        .with_interface(PROPS_IFACE)
        .at(bus_name)
        .build();
    call.body.push_param(SNI_IFACE).map_err(|e| e.to_string())?;

    let reply = do_call(conn, call)?;

    let mut parser = reply.body.parser();
    let param = parser.get_param().map_err(|e| e.to_string())?;
    let dict = match &param {
        Param::Container(Container::Dict(d)) => d,
        _ => return Err("expected dict from GetAll".into()),
    };

    let mut props = std::collections::HashMap::new();
    for (key, val) in &dict.map {
        let k = match key {
            Base::StringRef(s) => s.to_string(),
            Base::String(s) => s.clone(),
            _ => continue,
        };
        let v = extract_prop_string(val);
        props.insert(k, v);
    }

    let id = props.get("Id").cloned().unwrap_or_default();
    let title = props.get("Title").cloned().unwrap_or_default();

    Ok(ItemProps { props, id, title })
}

fn extract_prop_string(param: &Param) -> String {
    let inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        other => other,
    };
    match inner {
        Param::Base(Base::StringRef(s)) => s.to_string(),
        Param::Base(Base::String(s)) => s.clone(),
        Param::Base(Base::ObjectPathRef(p)) => p.to_string(),
        Param::Base(Base::ObjectPath(p)) => p.clone(),
        Param::Base(Base::Int32(i)) => i.to_string(),
        Param::Base(Base::Uint32(i)) => i.to_string(),
        Param::Base(Base::Boolean(b)) => {
            (if *b { "true" } else { "false" }).to_string()
        }
        Param::Base(Base::Byte(b)) => b.to_string(),
        Param::Base(Base::Int16(i)) => i.to_string(),
        Param::Base(Base::Uint16(i)) => i.to_string(),
        Param::Base(Base::Int64(i)) => i.to_string(),
        Param::Base(Base::Uint64(i)) => i.to_string(),
        Param::Container(Container::Struct(st)) => {
            let parts: Vec<String> = st.iter().map(extract_prop_string).collect();
            format!("({})", parts.join(", "))
        }
        Param::Container(Container::Array(arr)) => {
            format!("[{} element(s)]", arr.values.len())
        }
        other => format!("{other:?}"),
    }
}

fn collect_items(
    conn: &mut DuplexConn,
    addresses: &[(String, String)],
    args: &Args,
) -> Vec<(String, String, ItemProps)> {
    let mut items = Vec::new();
    for (bus_name, object_path) in addresses {
        if let Ok(props) = get_item_properties(conn, bus_name, object_path) {
            let match_name = args.name.as_ref().map_or(true, |n| props.title == *n);
            let match_id = args.id.as_ref().map_or(true, |n| props.id == *n);
            if match_name && match_id {
                items.push((bus_name.clone(), object_path.clone(), props));
            }
        }
    }
    items
}

fn print_items_brief(items: &[(String, String, ItemProps)]) {
    for (i, (addr, _path, props)) in items.iter().enumerate() {
        let title = if props.title.is_empty() { "(unnamed)" } else { &props.title };
        let id = &props.id;
        let cat = props.props.get("Category").map(|s| s.as_str()).unwrap_or("-");
        let st = props.props.get("Status").map(|s| s.as_str()).unwrap_or("-");
        println!("{}. {title}  [{cat}/{st}]  id={id}  addr={addr}", i + 1);
    }
}

fn build_item_lines<'a>(bus_name: &'a str, object_path: &'a str, props: &'a ItemProps) -> Vec<(&'a str, String)> {
    let mut lines: Vec<(&str, String)> = Vec::new();

    lines.push(("Bus name", bus_name.to_string()));
    lines.push(("Object path", object_path.to_string()));
    lines.push(("ID", props.id.clone()));
    lines.push(("Title", if props.title.is_empty() { "-".into() } else { props.title.clone() }));

    for key in [
        "Category", "Status", "WindowId", "IconName", "IconThemePath",
        "OverlayIconName", "AttentionIconName", "AttentionMovieName",
        "ItemIsMenu", "Menu",
    ] {
        if let Some(val) = props.props.get(key) {
            if !val.is_empty() && val != "0" {
                lines.push((key, val.clone()));
            }
        }
    }

    if let Some(tip) = props.props.get("ToolTip") {
        if tip.starts_with('(') && tip.ends_with(')') {
            let inner = &tip[1..tip.len()-1];
            let parts: Vec<&str> = inner.splitn(4, ", ").collect();
            if parts.len() == 4 {
                let title = parts[2].trim_matches(',');
                let desc = parts[3].trim_matches(',');
                if !title.is_empty() || !desc.is_empty() {
                    lines.push(("ToolTip", format!("icon={} title={} desc={}", parts[0], title, desc)));
                }
            }
        }
    }
    for key in ["IconPixmap", "OverlayIconPixmap", "AttentionIconPixmap"] {
        if let Some(val) = props.props.get(key) {
            lines.push((key, val.clone()));
        }
    }

    lines
}

fn get_menu_layout(
    conn: &mut DuplexConn,
    bus_name: &str,
    menu_path: &str,
) -> Result<Vec<MenuNode>, String> {
    let mut call = MessageBuilder::new()
        .call("GetLayout")
        .on(menu_path)
        .with_interface(MENU_IFACE)
        .at(bus_name)
        .build();
    call.body.push_param(0i32).map_err(|e| e.to_string())?;
    call.body.push_param(-1i32).map_err(|e| e.to_string())?;
    call.body
        .push_param(Vec::<&str>::new())
        .map_err(|e| e.to_string())?;

    let reply = do_call(conn, call)?;

    let mut parser = reply.body.parser();
    let _revision: u32 = parser.get().map_err(|e| e.to_string())?;
    let root = parser.get_param().map_err(|e| e.to_string())?;

    let fields = match &root {
        Param::Container(Container::Struct(s)) => s,
        _ => return Err("expected struct from GetLayout".into()),
    };
    if fields.len() < 3 {
        return Err("GetLayout struct too short".into());
    }
    let children = match &fields[2] {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return Err("expected array at field 2 of GetLayout result".into()),
    };

    Ok(children.iter().filter_map(parse_menu_node).collect())
}

#[derive(Debug)]
struct MenuNode {
    id: i32,
    label: String,
    enabled: bool,
    visible: bool,
    menu_type: String,
    toggle_type: String,
    toggle_state: i32,
    children: Vec<MenuNode>,
}

fn parse_menu_node(param: &Param) -> Option<MenuNode> {
    let inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        _ => return None,
    };
    let fields = match inner {
        Param::Container(Container::Struct(s)) => s.as_slice(),
        _ => return None,
    };
    if fields.len() < 3 {
        return None;
    }
    let id = match &fields[0] {
        Param::Base(Base::Int32(v)) => *v,
        _ => return None,
    };
    let props = match &fields[1] {
        Param::Container(Container::Dict(d)) => &d.map,
        _ => return None,
    };

    let get_str = |key: &str| -> String {
        get_variant_str(props, key)
    };
    let get_bool = |key: &str| -> Option<bool> {
        get_variant_bool(props, key)
    };
    let get_int = |key: &str| -> Option<i32> {
        get_variant_int(props, key)
    };

    let label = get_str("label");
    let enabled = get_bool("enabled").unwrap_or(true);
    let visible = get_bool("visible").unwrap_or(true);
    let menu_type = get_str("type");
    let toggle_type = get_str("toggle-type");
    let toggle_state = get_int("toggle-state").unwrap_or(-1);

    let children_param = match &fields[2] {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return None,
    };
    let children: Vec<MenuNode> = children_param.iter().filter_map(parse_menu_node).collect();

    Some(MenuNode {
        id,
        label,
        enabled,
        visible,
        menu_type,
        toggle_type,
        toggle_state,
        children,
    })
}

fn get_variant_str(props: &DictMap, key: &str) -> String {
    match get_variant_value(props, key) {
        Some(Param::Base(Base::StringRef(s))) => s.to_string(),
        Some(Param::Base(Base::String(s))) => s.clone(),
        _ => String::new(),
    }
}

fn get_variant_bool(props: &DictMap, key: &str) -> Option<bool> {
    match get_variant_value(props, key) {
        Some(Param::Base(Base::Boolean(b))) => Some(*b),
        _ => None,
    }
}

fn get_variant_int(props: &DictMap, key: &str) -> Option<i32> {
    match get_variant_value(props, key) {
        Some(Param::Base(Base::Int32(n))) => Some(*n),
        _ => None,
    }
}

fn get_variant_value<'a>(props: &'a DictMap, key: &str) -> Option<&'a Param<'a, 'a>> {
    for (k, v) in props {
        let k_str = match k {
            Base::StringRef(s) => *s,
            Base::String(s) => s.as_str(),
            _ => continue,
        };
        if k_str != key {
            continue;
        }
        if let Param::Container(Container::Variant(var)) = v {
            return Some(&var.value);
        }
    }
    None
}

fn print_menu_nodes(nodes: &[MenuNode], _width: usize, _fmt: &dyn Fn(&str, &str) -> String) {
    if nodes.is_empty() {
        println!("│ (empty)");
    } else {
        for node in nodes {
            print_menu_node(node, 1);
        }
    }
}

fn print_menu_node(node: &MenuNode, depth: usize) {
    let indent = "  ".repeat(depth);
    let prefix = format!("│{indent}");

    if node.menu_type == "separator" {
        println!("{prefix}─────────────────");
        return;
    }

    let label = if node.label.is_empty() { "(unnamed)" } else { &node.label };
    let flags = {
        let mut f = String::new();
        if !node.enabled { f.push_str(" [disabled]"); }
        if !node.visible { f.push_str(" [hidden]"); }
        match node.toggle_type.as_str() {
            "checkmark" => f.push_str(if node.toggle_state == 1 { " [✓]" } else { " [ ]" }),
            "radio" => f.push_str(if node.toggle_state == 1 { " [●]" } else { " [○]" }),
            _ => {}
        }
        f
    };

    println!("{prefix}ID:{} {label}{flags}", node.id);

    for child in &node.children {
        print_menu_node(child, depth + 1);
    }
}
