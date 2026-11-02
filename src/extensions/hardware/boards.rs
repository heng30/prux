//! 板卡静态资料表：芯片、描述与内存映射。
//!
//! 这些表是 datasheet 里能直接查到的事实，**探针不可用时**（没插板、没装驱动、
//! 或用户只是想问「这块板是什么芯片」）仍然要给得出答案——zeroclaw 的
//! `hardware_board_info` / `hardware_memory_map` 也是同样的回落顺序：
//! 先试探针实时读，失败再用静态表。
//!
//! 探针实时路径用的芯片名在 [`PROBE_CHIPS`]：probe-rs 的 target 名（如
//! `STM32F401RETx`），与展示用的完整型号（`STM32F401RET6`）不是一回事。

/// 一块板卡的静态资料。
#[derive(Debug, Clone, Copy)]
pub struct BoardSpec {
    /// 板卡名（配置里用的标识，如 `nucleo-f401re`）。
    pub board: &'static str,
    /// 展示用芯片完整型号。
    pub chip: &'static str,
    /// 一句话描述（内核、主频、存储、板载 LED 引脚）。
    pub description: &'static str,
    /// datasheet 里的内存映射（多行文本）。
    pub memory_map: &'static str,
    /// probe-rs 的 target 名；`None` = 本板不走探针通道。
    pub probe_chip: Option<&'static str>,
}

/// 已知板卡表（顺序即 `/hardware` 面板与兜底提示里的展示顺序）。
pub const BOARDS: &[BoardSpec] = &[
    BoardSpec {
        board: "nucleo-f401re",
        chip: "STM32F401RET6",
        description: "ARM Cortex-M4, 84 MHz. Flash: 512 KB, RAM: 128 KB. User LED on PA5 (pin 13).",
        memory_map: "Flash: 0x0800_0000 - 0x0807_FFFF (512 KB)\nRAM: 0x2000_0000 - 0x2001_FFFF (128 KB)",
        probe_chip: Some("STM32F401RETx"),
    },
    BoardSpec {
        board: "nucleo-f411re",
        chip: "STM32F411RET6",
        description: "ARM Cortex-M4, 100 MHz. Flash: 512 KB, RAM: 128 KB. User LED on PA5 (pin 13).",
        memory_map: "Flash: 0x0800_0000 - 0x0807_FFFF (512 KB)\nRAM: 0x2000_0000 - 0x2001_FFFF (128 KB)",
        probe_chip: Some("STM32F411RETx"),
    },
    BoardSpec {
        board: "arduino-uno",
        chip: "ATmega328P",
        description: "8-bit AVR, 16 MHz. Flash: 16 KB, SRAM: 2 KB. Built-in LED on pin 13.",
        memory_map: "Flash: 0x0000 - 0x3FFF (16 KB, ATmega328P)\nSRAM: 0x0100 - 0x08FF (2 KB)\nEEPROM: 0x0000 - 0x03FF (1 KB)",
        probe_chip: None,
    },
    BoardSpec {
        board: "arduino-mega",
        chip: "ATmega2560",
        description: "8-bit AVR, 16 MHz. Flash: 256 KB, SRAM: 8 KB. Built-in LED on pin 13.",
        memory_map: "Flash: 0x0000 - 0x3FFFF (256 KB, ATmega2560)\nSRAM: 0x0200 - 0x21FF (8 KB)\nEEPROM: 0x0000 - 0x0FFF (4 KB)",
        probe_chip: None,
    },
    BoardSpec {
        board: "arduino-uno-q",
        chip: "STM32U585 + Qualcomm",
        description: "Dual-core: STM32 (MCU) + Linux (aarch64). GPIO via Bridge app on port 9999.",
        memory_map: "MCU side: per STM32U585 datasheet\nLinux side: shared via the Bridge app",
        probe_chip: None,
    },
    BoardSpec {
        board: "esp32",
        chip: "ESP32",
        description: "Dual-core Xtensa LX6, 240 MHz. Flash: 4 MB typical. Built-in LED on GPIO 2.",
        memory_map: "Flash: 0x3F40_0000 - 0x3F7F_FFFF (4 MB typical)\nIRAM: 0x4000_0000 - 0x4005_FFFF\nDRAM: 0x3FFB_0000 - 0x3FFF_FFFF",
        probe_chip: None,
    },
    BoardSpec {
        board: "rpi-gpio",
        chip: "Raspberry Pi",
        description: "ARM Linux. Native GPIO via sysfs/rppal. No fixed LED pin.",
        memory_map: "Linux user space: 0x0000_0000 - 0x3FFF_FFFF (per-model)\nPeripheral base (BCM283x): 0x3F00_0000 (Pi 2/3) / 0xFE00_0000 (Pi 4+)",
        probe_chip: None,
    },
];

/// USB VID/PID → 板卡的映射（`available_ports` 里能拿到 VID/PID 时就靠它认板）。
///
/// 与 zeroclaw 的 registry 表同源：ST-Link 的 0x0483、Arduino 的 0x2341、CH340 的 0x1a86。
/// CP210x（0x10c4）这类**纯 USB-UART 桥**不在表里：认出来也没有板卡资料可给。
const USB_BOARDS: &[(u16, u16, &str)] = &[
    (0x0483, 0x374b, "nucleo-f401re"),
    (0x0483, 0x3748, "nucleo-f411re"),
    (0x2341, 0x0043, "arduino-uno"),
    (0x2341, 0x0078, "arduino-uno-q"),
    (0x2341, 0x0042, "arduino-mega"),
    (0x1a86, 0x7523, "esp32"),
    (0x1a86, 0x55d4, "esp32"),
];

/// 按 USB VID/PID 查板卡；未收录返回 `None`（不代表不能连，只是不认识）。
pub fn lookup_usb(vid: u16, pid: u16) -> Option<&'static BoardSpec> {
    let board = USB_BOARDS
        .iter()
        .find(|(v, p, _)| *v == vid && *p == pid)?
        .2;
    lookup(board)
}

/// 按板卡名查静态资料（大小写不敏感，允许带前后空白）。
pub fn lookup(board: &str) -> Option<&'static BoardSpec> {
    let wanted = board.trim().to_lowercase();
    BOARDS.iter().find(|spec| spec.board == wanted)
}

/// 全部板卡名（配置错误提示与 `/hardware` 面板用）。
pub fn names() -> Vec<&'static str> {
    BOARDS.iter().map(|spec| spec.board).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(lookup(" NUCLEO-F401RE ").unwrap().chip, "STM32F401RET6");
        assert!(lookup("no-such-board").is_none());
    }

    #[test]
    fn every_board_has_a_documented_memory_map() {
        for spec in BOARDS {
            assert!(!spec.memory_map.is_empty(), "{} 缺少内存映射", spec.board);
            assert!(!spec.description.is_empty(), "{} 缺少描述", spec.board);
        }
    }

    #[test]
    fn usb_ids_map_to_known_boards() {
        assert_eq!(lookup_usb(0x0483, 0x374b).unwrap().board, "nucleo-f401re");
        assert_eq!(lookup_usb(0x1a86, 0x7523).unwrap().board, "esp32");
        assert!(lookup_usb(0xffff, 0xffff).is_none());
        // USB 表里出现的每个板卡名都必须在 BOARDS 里（否则面板会显示空资料）
        for (_, _, board) in USB_BOARDS {
            assert!(lookup(board).is_some(), "USB 表引用了未知板卡 {board}");
        }
    }

    #[test]
    fn probe_capable_boards_use_probe_rs_target_names() {
        let f401 = lookup("nucleo-f401re").unwrap();
        assert_eq!(f401.probe_chip, Some("STM32F401RETx"));
        assert!(lookup("arduino-uno").unwrap().probe_chip.is_none());
    }
}
