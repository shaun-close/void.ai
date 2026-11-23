//! The app's menu actions, in one table: the menu bar draws its items from it and the palette
//! lists every entry (`>` narrows the palette to them). `VoidApp::run_command` does them.

use crate::theme::icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandId {
    OpenFolder,
    Scratch,
    NewChat,
    Quit,
    NewChatTab,
    ShowChats,
    ShowFiles,
    ShowRuns,
    ShowBackends,
    ShowSettings,
    ShowReview,
    ResetLayout,
    RerunLast,
}

pub struct CommandDef {
    pub id: CommandId,
    pub icon: &'static str,
    pub label: &'static str,
    /// The menu it sits in (the palette shows it as the command's detail).
    pub menu: &'static str,
    pub keys: Option<&'static str>,
}

const fn cmd(
    id: CommandId,
    menu: &'static str,
    icon: &'static str,
    label: &'static str,
) -> CommandDef {
    CommandDef {
        id,
        icon,
        label,
        menu,
        keys: None,
    }
}

pub const COMMANDS: &[CommandDef] = &[
    cmd(
        CommandId::OpenFolder,
        "File",
        icon::FOLDER_OPEN,
        "Open Folder…",
    ),
    cmd(CommandId::Scratch, "File", icon::PLANET, "Scratch"),
    cmd(CommandId::NewChat, "File", icon::PLUS, "New chat"),
    cmd(CommandId::Quit, "File", icon::SIGN_OUT, "Quit"),
    cmd(CommandId::NewChatTab, "View", icon::CHAT, "New chat tab"),
    cmd(CommandId::ShowChats, "View", icon::CHATS, "Chats"),
    cmd(CommandId::ShowFiles, "View", icon::FOLDER, "Files"),
    cmd(CommandId::ShowRuns, "View", icon::PLAY, "Runs"),
    cmd(CommandId::ShowBackends, "View", icon::PLUGS, "Backends"),
    cmd(CommandId::ShowSettings, "View", icon::GEAR, "Settings"),
    cmd(CommandId::ShowReview, "View", icon::GIT_DIFF, "Review"),
    cmd(CommandId::ResetLayout, "View", icon::LAYOUT, "Reset layout"),
    CommandDef {
        keys: Some("Ctrl+Shift+F10"),
        ..cmd(
            CommandId::RerunLast,
            "Run",
            icon::ARROW_CLOCKWISE,
            "Re-run last",
        )
    },
];

pub fn def(id: CommandId) -> &'static CommandDef {
    COMMANDS
        .iter()
        .find(|c| c.id == id)
        .expect("every command is in the table")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_is_in_the_table_once() {
        for c in COMMANDS {
            assert_eq!(
                COMMANDS.iter().filter(|d| d.id == c.id).count(),
                1,
                "{:?}",
                c.id
            );
            assert_eq!(def(c.id).label, c.label);
        }
    }
}
