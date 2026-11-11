use std::sync::mpsc::{self, Receiver, Sender};
use tray_icon::Icon;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

const SHOW_ID: &str = "raja-show";
const EXIT_ID: &str = "raja-exit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Show,
    Exit,
}

pub struct TrayHandle {
    _icon: TrayIcon,
    receiver: Receiver<TrayCommand>,
}

impl TrayHandle {
    pub fn new() -> Result<Self, String> {
        let menu = Menu::new();
        let show = MenuItem::with_id(SHOW_ID, "Show RAJA", true, None);
        let exit = MenuItem::with_id(EXIT_ID, "Exit RAJA", true, None);
        menu.append(&show).map_err(|error| error.to_string())?;
        menu.append(&exit).map_err(|error| error.to_string())?;

        let (sender, receiver) = mpsc::channel();
        let event_sender: Sender<TrayCommand> = sender;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = if event.id == SHOW_ID {
                Some(TrayCommand::Show)
            } else if event.id == EXIT_ID {
                Some(TrayCommand::Exit)
            } else {
                None
            };
            if let Some(command) = command {
                let _ = event_sender.send(command);
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_tooltip("RAJA Download Manager")
            .with_icon(raja_icon()?)
            .with_menu(Box::new(menu))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            _icon: icon,
            receiver,
        })
    }

    pub fn try_recv(&self) -> Result<TrayCommand, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

const SHIELD_64_RGBA: &[u8] = include_bytes!("tray_shield_64.rgba");

fn raja_icon() -> Result<Icon, String> {
    const SIZE: usize = 64;
    let pixels = render_3d_tray_icon(SIZE);
    Icon::from_rgba(pixels, SIZE as u32, SIZE as u32).map_err(|error| error.to_string())
}

pub fn render_3d_tray_icon(size: usize) -> Vec<u8> {
    if size == 64 && SHIELD_64_RGBA.len() == 64 * 64 * 4 {
        return SHIELD_64_RGBA.to_vec();
    }
    let mut pixels = vec![0u8; size * size * 4];
    for py in 0..size {
        let sy = (py * 64 / size.max(1)).min(63);
        for px in 0..size {
            let sx = (px * 64 / size.max(1)).min(63);
            let src_idx = (sy * 64 + sx) * 4;
            let dst_idx = (py * size + px) * 4;
            pixels[dst_idx..dst_idx + 4].copy_from_slice(&SHIELD_64_RGBA[src_idx..src_idx + 4]);
        }
    }
    pixels
}
