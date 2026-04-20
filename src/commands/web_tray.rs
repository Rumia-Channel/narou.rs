use std::mem::zeroed;

use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, TranslateMessage,
};

use super::send_control_request;

pub fn spawn_web_tray(host: String, port: u16, control_token: String) {
    let _ = std::thread::Builder::new()
        .name("narou-web-tray".to_string())
        .spawn(move || {
            if let Err(error) = run_web_tray(host, port, control_token) {
                eprintln!("warning: failed to start web tray: {}", error);
            }
        });
}

fn run_web_tray(host: String, port: u16, control_token: String) -> Result<(), String> {
    let menu = Menu::new();
    let restart_item = MenuItem::new("再起動", true, None);
    let exit_item = MenuItem::new("終了", true, None);
    menu.append_items(&[&restart_item, &exit_item])
        .map_err(|error| error.to_string())?;

    let restart_id = restart_item.id().clone();
    let exit_id = exit_item.id().clone();
    let action_host = host.clone();
    let action_token = control_token.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let endpoint = if event.id == restart_id {
            Some("/api/reboot")
        } else if event.id == exit_id {
            Some("/api/shutdown")
        } else {
            None
        };
        if let Some(endpoint) = endpoint {
            if send_control_request(&action_host, port, Some(&action_token), endpoint) {
                unsafe {
                    PostQuitMessage(0);
                }
            }
        }
    }));

    let _tray_icon = TrayIconBuilder::new()
        .with_tooltip("narou_rs web")
        .with_icon(build_tray_icon()?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .build()
        .map_err(|error| error.to_string())?;

    run_message_loop();
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    Ok(())
}

fn build_tray_icon() -> Result<Icon, String> {
    const SIDE: u32 = 32;
    const BG: [u8; 4] = [0x2b, 0x4c, 0x7e, 0xff];
    const FG: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    const TRANSPARENT: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
    const RADIUS: i32 = 6;

    let mut rgba = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            rgba.extend_from_slice(if rounded_rect_contains(x as i32, y as i32, SIDE as i32) {
                &BG
            } else {
                &TRANSPARENT
            });
        }
    }

    for y in 7..25 {
        fill_rect(&mut rgba, SIDE, 8, y, 3, 1, FG);
        fill_rect(&mut rgba, SIDE, 20, y, 3, 1, FG);
        let offset = y - 7;
        let diagonal_x = 10 + (offset * 10) / 17;
        fill_rect(&mut rgba, SIDE, diagonal_x, y, 3, 1, FG);
    }

    fn rounded_rect_contains(x: i32, y: i32, side: i32) -> bool {
        let right = side - 1;
        let bottom = side - 1;
        if (x >= RADIUS && x <= right - RADIUS) || (y >= RADIUS && y <= bottom - RADIUS) {
            return true;
        }
        let cx = if x < RADIUS { RADIUS } else { right - RADIUS };
        let cy = if y < RADIUS { RADIUS } else { bottom - RADIUS };
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= RADIUS * RADIUS
    }

    fn fill_rect(
        rgba: &mut [u8],
        side: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        color: [u8; 4],
    ) {
        for yy in y..(y + height) {
            for xx in x..(x + width) {
                let index = ((yy * side + xx) * 4) as usize;
                rgba[index..index + 4].copy_from_slice(&color);
            }
        }
    }

    Icon::from_rgba(rgba, SIDE, SIDE).map_err(|error| error.to_string())
}

fn run_message_loop() {
    unsafe {
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
