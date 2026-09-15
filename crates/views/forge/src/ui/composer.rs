//! Draft and upload decisions belong to the conversation guest.
use super::{ForgeView, Message};
use ducktape_view_composer::host::{self, SelectedFile, Target};
use ducktape_view_composer::{self as composer, Attachment, AttachmentState, Event, Outcome, Send};
use ducktape_view_guest::{Task, widget, wire};

#[derive(Clone, Hash)]
pub struct Route {
    scope: String,
    key: String,
    target: Target,
    connection: i64,
}
#[derive(Clone)]
pub enum ComposerMessage {
    Editor(Route, Box<Event<Message>>),
    Picked(Route, Result<Vec<SelectedFile>, String>),
    Uploaded(Route, String, Result<String, String>),
    Clipboard(Route, Result<host::Clipboard, String>),
    Prepared(Route, Send, Result<String, String>),
    Sent(Route, String, Send, Result<(), String>),
    Copied(Route, Result<(), String>),
}
impl ForgeView {
    pub(crate) fn composer(
        &self,
        key: String,
        scope: String,
        target: Target,
        hint: &str,
        editable: bool,
    ) -> wire::Node {
        let draft = self.composers.get(&scope).cloned().unwrap_or_default();
        let choices = self.composer_choices.clone();
        let route = Route {
            scope,
            key: key.clone(),
            target,
            connection: self.connection_serial,
        };
        composer::view(&draft, &key, hint, editable, &choices, move |event| {
            Message::Composer(Box::new(ComposerMessage::Editor(
                route.clone(),
                Box::new(event),
            )))
        })
    }
    pub(crate) fn on_composer(&mut self, message: ComposerMessage) -> Task<Message> {
        match message {
            ComposerMessage::Editor(route, event) => self.composer_editor(route, *event),
            ComposerMessage::Picked(route, result) => self.composer_picked(route, result),
            ComposerMessage::Uploaded(route, token, result) => {
                self.composer_uploaded(route, token, result)
            }
            ComposerMessage::Clipboard(route, result) => self.composer_clipboard(route, result),
            ComposerMessage::Prepared(route, send, result) => {
                self.composer_prepared(route, send, result)
            }
            ComposerMessage::Sent(route, id, send, result) => {
                self.composer_sent(route, id, send, result)
            }
            ComposerMessage::Copied(route, result) => self.composer_copied(route, result),
        }
    }
    fn composer_editor(&mut self, route: Route, event: Event<Message>) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let choices = self.composer_choices.clone();
        let draft = self.composers.entry(route.scope.clone()).or_default();
        match draft.handle(event, &choices) {
            Outcome::Updated => Task::none(),
            Outcome::Message(message) => self.update(message),
            Outcome::Enqueue(tag) => widget::perform(wire::WidgetCommand::EditorAction {
                target: format!("{}/editor", route.key),
                tag,
            }),
            Outcome::Action(tag) => self.composer_action(route, &tag),
        }
    }
    fn composer_action(&mut self, route: Route, tag: &str) -> Task<Message> {
        if let Some(token) = tag.strip_prefix("remove:") {
            if let Some(handle) = self.upload_handles.remove(token) {
                handle.abort();
            }
            self.composers
                .entry(route.scope.clone())
                .or_default()
                .attachments
                .retain(|file| file.token != token);
            let token = token.to_owned();
            return Task::perform(
                async move {
                    host::release(&token).await;
                },
                move |()| {
                    Message::Composer(Box::new(ComposerMessage::Copied(route.clone(), Ok(()))))
                },
            );
        }
        if let Some(token) = tag.strip_prefix("retry:") {
            let draft = self.composers.entry(route.scope.clone()).or_default();
            let Some(index) = draft.attachments.iter().position(|file| {
                file.token == token && matches!(file.state, AttachmentState::Failed { .. })
            }) else {
                return Task::none();
            };
            let file = draft.attachments.remove(index);
            return self.composer_picked(
                route,
                Ok(vec![SelectedFile {
                    token: file.token,
                    name: file.name,
                    bytes: file.bytes,
                }]),
            );
        }
        match tag {
            "send" => self.composer_send(route),
            "attach" => {
                if matches!(route.target, Target::Edit { .. }) {
                    return Task::none();
                }
                Task::perform(host::pick(), move |result| {
                    Message::Composer(Box::new(ComposerMessage::Picked(route.clone(), result)))
                })
            }
            "paste" => Task::perform(host::clipboard(), move |result| {
                Message::Composer(Box::new(ComposerMessage::Clipboard(route.clone(), result)))
            }),
            "copy" | "cut" => {
                let Some(text) = self
                    .composers
                    .entry(route.scope.clone())
                    .or_default()
                    .clipboard
                    .take()
                else {
                    return Task::none();
                };
                Task::future(async move {
                    let result = host::copy(&text).await;
                    Message::Composer(Box::new(ComposerMessage::Copied(route, result)))
                })
            }
            _ => Task::none(),
        }
    }
    fn composer_admitted(&self, route: &Route) -> bool {
        let channel = match &route.target {
            Target::Post { channel, .. } | Target::Edit { channel, .. } => channel,
        };
        self.connected
            && route.connection == self.connection_serial
            && channel == &self.forge_item_channel
            && self.item_phase == "ready"
    }
    fn composer_send(&mut self, route: Route) -> Task<Message> {
        let admitted = self.composer_admitted(&route);
        let draft = self.composers.entry(route.scope.clone()).or_default();
        let Some(send) = draft.submitted.take() else {
            return Task::none();
        };
        if !admitted {
            draft.failed(send);
            draft.note = "The room changed before this message was sent".into();
            return Task::none();
        }

        draft.in_flight.push(send.clone());
        Task::future(async move {
            let result = host::id().await;
            Message::Composer(Box::new(ComposerMessage::Prepared(route, send, result)))
        })
    }
    fn composer_prepared(
        &mut self,
        route: Route,
        send: Send,
        result: Result<String, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if !self.composer_admitted(&route) {
            return self.composer_sent(
                route,
                String::new(),
                send,
                Err("The room changed before this message was sent".into()),
            );
        }
        let id = match result {
            Ok(id) => id,
            Err(error) => return self.composer_sent(route, String::new(), send, Err(error)),
        };
        Task::future(async move {
            let result = host::submit(id.clone(), &send, &route.target).await;
            Message::Composer(Box::new(ComposerMessage::Sent(route, id, send, result)))
        })
    }
    fn composer_picked(
        &mut self,
        route: Route,
        result: Result<Vec<SelectedFile>, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        let files = match result {
            Ok(files) => files,
            Err(error) => {
                self.composers.entry(route.scope).or_default().note = error;
                return Task::none();
            }
        };
        let draft = self.composers.entry(route.scope.clone()).or_default();
        let mut tasks = Vec::new();
        for file in files {
            draft.attachments.push(Attachment {
                token: file.token.clone(),
                name: file.name.clone(),
                bytes: file.bytes,
                state: AttachmentState::Uploading,
            });
            let route = route.clone();
            let upload_token = file.token.clone();
            let (task, handle) = Task::future(async move {
                let token = file.token.clone();
                let result = host::upload(file).await;
                Message::Composer(Box::new(ComposerMessage::Uploaded(route, token, result)))
            })
            .abortable();
            self.upload_handles
                .insert(upload_token, handle.abort_on_drop());
            tasks.push(task);
        }
        Task::batch(tasks)
    }
    fn composer_uploaded(
        &mut self,
        route: Route,
        token: String,
        result: Result<String, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        self.upload_handles.remove(&token);
        let Some(draft) = self.composers.get_mut(&route.scope) else {
            return Task::none();
        };
        if let Some(file) = draft
            .attachments
            .iter_mut()
            .find(|file| file.token == token)
        {
            file.state = match result {
                Ok(uri) => AttachmentState::Ready { uri },
                Err(reason) => AttachmentState::Failed { reason },
            };
        }
        Task::none()
    }
    fn composer_clipboard(
        &mut self,
        route: Route,
        result: Result<host::Clipboard, String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if !self.composer_admitted(&route) {
            return Task::none();
        }
        let clipboard = match result {
            Ok(clipboard) => clipboard,
            Err(error) => {
                self.composers.entry(route.scope).or_default().note = error;
                return Task::none();
            }
        };
        self.composers.entry(route.scope.clone()).or_default().paste = Some(clipboard.text);
        let paste = widget::perform(wire::WidgetCommand::EditorAction {
            target: format!("{}/editor", route.key),
            tag: "paste-ready".into(),
        });
        let files = match route.target {
            Target::Post { .. } => self.composer_picked(route, Ok(clipboard.files)),
            Target::Edit { .. } => Task::none(),
        };
        Task::batch([paste, files])
    }
    fn composer_copied(&mut self, route: Route, result: Result<(), String>) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if let Err(error) = result {
            self.composers.entry(route.scope).or_default().note = error;
        }
        Task::none()
    }
    fn composer_sent(
        &mut self,
        route: Route,
        _id: String,
        send: Send,
        result: Result<(), String>,
    ) -> Task<Message> {
        if route.connection != self.connection_serial {
            return Task::none();
        }
        if let Some(draft) = self.composers.get_mut(&route.scope) {
            draft.complete_send(&send);
        }
        match result {
            Ok(()) => {}
            Err(error) => {
                let draft = self.composers.entry(route.scope).or_default();
                draft.failed(send);
                draft.note = error;
            }
        }
        Task::none()
    }
}
