use super::{Workspace as IronWorkspace, WorkspaceClient, WorkspaceUpdate};
use crate::channels::SyncSenderExt;
use crate::clients::compositor::Visibility;
use crate::{arc_rw, read_lock, spawn, write_lock};
use connection::{Action, Connection, Event, Request, Response, Window};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;
use tracing::{debug, error};

mod connection;

#[derive(Debug)]
pub struct Client {
    tx: broadcast::Sender<WorkspaceUpdate>,
    _rx: broadcast::Receiver<WorkspaceUpdate>,

    // We store "windows" in the "workspaces" state variable
    // because we are mapping windows -> IronWorkspace
    windows_as_workspaces: Arc<RwLock<Vec<IronWorkspace>>>,
}

impl Client {
    pub fn new() -> Self {
        let (tx, rx) = broadcast::channel(32);
        let tx2 = tx.clone();

        let window_state = arc_rw!(vec![]);
        let window_state2 = window_state.clone();

        spawn(async move {
            let mut conn = Connection::connect().await?;
            let (_, mut event_listener) = conn.send(Request::EventStream).await?;

            // Initial fetch
            Self::refresh_windows(&tx, &window_state, true).await;

            loop {
                // We just listen for events. If *anything* relevant changes, we fetch the full window list.
                // This is robust against drift and handles the sorting automatically since Niri
                // returns windows in order.
                let event = match event_listener() {
                    Ok(event) => event,
                    Err(err) => {
                        error!("Niri connection error: {err:?}");
                        break;
                    }
                };

                let should_refresh = matches!(
                    event,
                    Event::WindowsChanged { .. }
                        | Event::WindowOpenedOrChanged { .. }
                        | Event::WindowClosed { .. }
                        | Event::WindowFocusChanged { .. }
                        | Event::WorkspacesChanged { .. }
                        | Event::WorkspaceActivated { .. }
                );

                if should_refresh {
                    Self::refresh_windows(&tx, &window_state, false).await;
                }
            }

            Ok::<(), std::io::Error>(())
        });

        Self {
            tx: tx2,
            _rx: rx,
            windows_as_workspaces: window_state2,
        }
    }

    /// Fetches the list of windows from Niri and updates the Ironbar state.
    async fn refresh_windows(
        tx: &broadcast::Sender<WorkspaceUpdate>,
        state_lock: &Arc<RwLock<Vec<IronWorkspace>>>,
        is_init: bool,
    ) {
        // We need a separate connection to send requests while the other is listening
        let windows = match Connection::connect().await {
            Ok(mut conn) => match conn.send(Request::Windows).await {
                Ok((Ok(Response::Windows(windows)), _)) => windows,
                Ok((Err(e), _)) => {
                    error!("Failed to fetch windows: {e}");
                    return;
                }
                Err(e) => {
                    error!("Failed to send window request: {e}");
                    return;
                }
                _ => return,
            },
            Err(e) => {
                error!("Failed to connect to Niri for refresh: {e}");
                return;
            }
        };

        // Convert Niri Windows to IronWorkspaces
        // Niri returns windows in the correct order (workspace index, then id)
        let new_workspaces: Vec<IronWorkspace> = windows
            .iter()
            .enumerate()
            .map(|(idx, w)| Self::window_to_workspace(w, idx as i64))
            .collect();

        let mut updates: Vec<WorkspaceUpdate> = vec![];

        if is_init {
            updates.push(WorkspaceUpdate::Init(new_workspaces.clone()));
        } else {
            let old_state = read_lock!(state_lock);

            // 1. Check for Add/Update/Move
            for new_w in &new_workspaces {
                if let Some(old_w) = old_state.iter().find(|w| w.id == new_w.id) {
                    // Check rename
                    if new_w.name != old_w.name {
                        updates.push(WorkspaceUpdate::Rename {
                            id: new_w.id,
                            name: new_w.name.clone(),
                        });
                    }
                    // Check move (index changed or monitor changed)
                    if new_w.index != old_w.index || new_w.monitor != old_w.monitor {
                        updates.push(WorkspaceUpdate::Move(new_w.clone()));
                    }
                    // Check focus change
                    if new_w.visibility.is_focused() != old_w.visibility.is_focused() {
                        // We handle focus update specifically
                        if new_w.visibility.is_focused() {
                             updates.push(WorkspaceUpdate::Focus {
                                old: Some(old_w.clone()), // The previously known state of this window
                                new: new_w.clone()
                            });
                        }
                    }
                } else {
                    updates.push(WorkspaceUpdate::Add(new_w.clone()));
                }
            }

            // 2. Check for Remove
            for old_w in old_state.iter() {
                if !new_workspaces.iter().any(|w| w.id == old_w.id) {
                    updates.push(WorkspaceUpdate::Remove(old_w.id));
                }
            }
        }

        // Apply updates
        *write_lock!(state_lock) = new_workspaces;
        for update in updates {
            tx.send_expect(update);
        }
    }

    fn window_to_workspace(window: &Window, index: i64) -> IronWorkspace {
        let is_focused = window.is_focused;
        
        // We map the window ID to the workspace ID so buttons track windows.
        // We use the window title (or app_id if title is empty) as the name.
        let name = window
            .title
            .clone()
            .or_else(|| window.app_id.clone())
            .unwrap_or_else(|| "Window".to_string());

        IronWorkspace {
            id: window.id as i64,
            index, // Use the visual index from the list
            name,
            monitor: window.output.clone().unwrap_or_default(),
            visibility: if is_focused {
                Visibility::focused()
            } else {
                Visibility::visible()
            },
        }
    }
}

impl WorkspaceClient for Client {
    fn focus(&self, id: i64) {
        debug!("focusing window with id: {}", id);

        spawn(async move {
            let mut conn = match Connection::connect().await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to connect to Niri for focus: {e}");
                    return;
                }
            };

            // We are focusing a WINDOW, so we use Action::FocusWindow
            let command = Request::Action(Action::FocusWindow {
                id: id as u64,
            });

            if let Err(err) = conn.send(command).await {
                error!("failed to send focus command: {err:?}");
            }
        });
    }

    fn subscribe(&self) -> broadcast::Receiver<WorkspaceUpdate> {
        let rx = self.tx.subscribe();

        let windows = read_lock!(self.windows_as_workspaces);
        if !windows.is_empty() {
            self.tx
                .send_expect(WorkspaceUpdate::Init(windows.clone()));
        }

        rx
    }
}
