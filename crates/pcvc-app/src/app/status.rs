//! Status view: server state, addresses, mDNS, clients, versions and logs.

use std::time::SystemTime;

use iced::widget::{button, column, container, row, scrollable, svg, text};
use iced::{Center, Element, Fill, FillPortion, Font};
use pcvc_protocol::{APPLICATION_VERSION, PROTOCOL_VERSION};
use pcvc_server::MdnsStatus;

use super::{App, Message};
use crate::server_ctl::ServerStatus;
use crate::style::{self, Button};
use crate::{icons, model};

const LOG_LINES_SHOWN: usize = 8;

impl App {
    pub(super) fn status_view(&self) -> Element<'_, Message> {
        let clients = self.clients.len();
        let (icon, headline, detail) = match &self.server_status {
            ServerStatus::Running(_) if clients > 0 => (
                icons::TRAY_CLIENTS,
                "Online",
                model::clients_label(clients) + " connected",
            ),
            ServerStatus::Running(_) => {
                (icons::TRAY_ONLINE, "Online", "No clients connected".into())
            }
            ServerStatus::Starting => (icons::TRAY_OFFLINE, "Starting", String::new()),
            ServerStatus::Stopped => (icons::TRAY_OFFLINE, "Offline", "Server stopped".into()),
            ServerStatus::Failed(e) => (icons::TRAY_OFFLINE, "Offline", e.clone()),
        };
        let running = matches!(
            self.server_status,
            ServerStatus::Running(_) | ServerStatus::Starting
        );
        let toggle = button(text(if running {
            "Stop server"
        } else {
            "Start server"
        }))
        .padding([style::SPACE_S, style::SPACE_L])
        .on_press(Message::ToggleServer)
        .style(style::button(if running {
            Button::Danger
        } else {
            Button::Success
        }));
        let indicator = container(
            row![
                svg(svg::Handle::from_memory(icon))
                    .width(style::STATUS_ICON)
                    .height(style::STATUS_ICON),
                column![
                    text(headline).size(style::TEXT_HEADLINE),
                    text(detail)
                        .size(style::TEXT_SMALL)
                        .style(style::muted_text),
                ]
                .spacing(style::SPACE_XS)
                .width(Fill),
                toggle,
            ]
            .spacing(style::SPACE_L)
            .align_y(Center),
        )
        .padding(style::SPACE_L)
        .style(style::card);

        let addresses: Element<'_, Message> = match &self.server_status {
            ServerStatus::Running(addr) => column(
                model::listen_addresses(*addr, &self.local_ips)
                    .into_iter()
                    .map(|a| text(a).font(Font::MONOSPACE).into()),
            )
            .spacing(style::SPACE_XS)
            .into(),
            _ => text("Not listening").into(),
        };
        let mdns = match &self.mdns {
            MdnsStatus::Disabled => "Not advertised".to_owned(),
            MdnsStatus::Starting => "Registering".to_owned(),
            MdnsStatus::Registered { instance } => format!("Advertised as {instance}"),
            MdnsStatus::Failed(e) => format!("Failed: {e}"),
        };

        let now = SystemTime::now();
        let client_rows: Element<'_, Message> = if self.clients.is_empty() {
            text("None").into()
        } else {
            let header = row![
                text("Address")
                    .size(style::TEXT_SMALL)
                    .style(style::muted_text)
                    .width(FillPortion(3)),
                text("Connected since")
                    .size(style::TEXT_SMALL)
                    .style(style::muted_text)
                    .width(FillPortion(2)),
            ];
            column(
                std::iter::once(header.into()).chain(self.clients.iter().map(|c| {
                    row![
                        text(c.addr.to_string())
                            .font(Font::MONOSPACE)
                            .width(FillPortion(3)),
                        text(model::connected_since(c.connected_since, now)).width(FillPortion(2)),
                    ]
                    .into()
                })),
            )
            .spacing(style::SPACE_XS)
            .into()
        };

        let logs = column(self.logs.recent(LOG_LINES_SHOWN).into_iter().map(|l| {
            text(l.display())
                .font(Font::MONOSPACE)
                .size(style::TEXT_SMALL)
                .into()
        }))
        .spacing(2);

        let versions = format!(
            "PcVolumeControl {} (reports {APPLICATION_VERSION}), protocol v{PROTOCOL_VERSION}",
            env!("CARGO_PKG_VERSION"),
        );

        scrollable(
            column![
                indicator,
                section("Listening on", addresses),
                section("mDNS", text(mdns)),
                section("Connected clients", client_rows),
                section(
                    "Recent log",
                    container(logs)
                        .padding(style::SPACE_M)
                        .width(Fill)
                        .style(style::card)
                ),
                text(versions)
                    .size(style::TEXT_SMALL)
                    .style(style::muted_text),
            ]
            .spacing(style::SPACE_L)
            .padding(iced::Padding::ZERO.right(style::SPACE_M)),
        )
        .into()
    }
}

fn section<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![
        text(title).size(style::TEXT_SMALL).style(style::muted_text),
        body.into()
    ]
    .spacing(style::SPACE_S)
    .into()
}
