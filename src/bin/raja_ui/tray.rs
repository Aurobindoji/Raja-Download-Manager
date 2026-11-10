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

fn raja_icon() -> Result<Icon, String> {
    const SIZE: usize = 32;
    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as isize - 16;
            let dy = y as isize - 16;
            if dx * dx + dy * dy <= 15 * 15 {
                let index = (y * SIZE + x) * 4;
                pixels[index..index + 4].copy_from_slice(&[233, 104, 36, 255]);
            }
        }
    }
    for (x, y) in [
        (10, 8),
        (11, 8),
        (12, 8),
        (13, 8),
        (14, 8),
        (15, 8),
        (16, 8),
        (17, 8),
        (18, 8),
        (19, 8),
        (20, 8),
        (10, 9),
        (20, 9),
        (10, 10),
        (20, 10),
        (10, 11),
        (11, 11),
        (12, 11),
        (13, 11),
        (14, 11),
        (15, 11),
        (16, 11),
        (17, 11),
        (18, 11),
        (19, 11),
        (10, 12),
        (11, 12),
        (19, 12),
        (20, 12),
        (10, 13),
        (11, 13),
        (19, 13),
        (20, 13),
        (10, 14),
        (11, 14),
        (12, 14),
        (13, 14),
        (14, 14),
        (15, 14),
        (16, 14),
        (17, 14),
        (18, 14),
        (19, 14),
        (20, 14),
        (10, 15),
        (11, 15),
        (12, 15),
        (13, 15),
        (14, 15),
        (15, 15),
        (16, 15),
        (17, 15),
        (18, 15),
        (19, 15),
        (20, 15),
        (12, 16),
        (13, 16),
        (18, 16),
        (19, 16),
        (13, 17),
        (14, 17),
        (17, 17),
        (18, 17),
        (14, 18),
        (15, 18),
        (16, 18),
        (17, 18),
        (15, 19),
        (16, 19),
        (16, 20),
        (17, 20),
        (17, 21),
        (18, 21),
        (18, 22),
        (19, 22),
        (19, 23),
        (20, 23),
        (20, 24),
    ] {
        let index = (y * SIZE + x) * 4;
        pixels[index..index + 4].copy_from_slice(&[255, 255, 255, 255]);
    }
    Icon::from_rgba(pixels, SIZE as u32, SIZE as u32).map_err(|error| error.to_string())
}
