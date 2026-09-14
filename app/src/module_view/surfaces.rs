//! Native controls occupy only the surface names admitted for the mounted guest.
//! Their events keep the same instance identity as ordinary wire-tree controls.

use super::*;
use gpui_kit::{AnyView, Context, Entity, EventEmitter, Subscription, Window};

pub(super) struct Surface {
    name: String,
    args: Vec<wire::SurfaceValue>,
    handler: Option<u32>,
    view: View,
    _events: Option<Subscription>,
    _observations: Option<Subscription>,
}

enum View {
    Composer(Entity<crate::composer_surface::ComposerView>),
    Markdown(Entity<crate::backend::MarkdownSurface>),
    Picture(Entity<crate::backend::PictureView>),
    Code(Entity<crate::backend::CodeView>),
    Asset(Entity<crate::view_tree::ViewTree>),
}

impl View {
    fn any(&self) -> AnyView {
        match self {
            Self::Composer(view) => view.clone().into(),
            Self::Markdown(view) => view.clone().into(),
            Self::Picture(view) => view.clone().into(),
            Self::Code(view) => view.clone().into(),
            Self::Asset(view) => view.clone().into(),
        }
    }

    fn replace(
        &self,
        name: &str,
        args: &[wire::SurfaceValue],
        guest: &Guest,
        window: &mut Window,
        cx: &mut Context<NativeModuleView>,
    ) -> Result<(), String> {
        match self {
            Self::Composer(view) => view.update(cx, |view, cx| view.replace(args, window, cx)),
            Self::Markdown(view) => {
                let (source, doc, dark) = markdown_args(name, args);
                view.update(cx, |view, cx| view.replace(source, doc, dark, cx));
                Ok(())
            }
            Self::Picture(view) => {
                view.update(cx, |view, cx| {
                    view.replace(surface_str(args, 0), surface_str(args, 1), cx)
                });
                Ok(())
            }
            Self::Code(view) => {
                view.update(cx, |view, cx| {
                    view.replace(
                        surface_str(args, 0),
                        surface_str(args, 1),
                        surface_bool(args, 2),
                        window,
                        cx,
                    )
                });
                Ok(())
            }
            Self::Asset(view) => {
                let root = asset_node(name, args, guest);
                view.update(cx, |view, cx| view.replace(root, cx));
                Ok(())
            }
        }
    }
}

fn markdown_args(name: &str, args: &[wire::SurfaceValue]) -> (String, String, bool) {
    match name {
        "agent_markdown" => (surface_str(args, 0), String::new(), surface_bool(args, 1)),
        _ => (
            surface_str(args, 0),
            surface_str(args, 1),
            surface_bool(args, 2),
        ),
    }
}

pub(super) fn asset_node(name: &str, args: &[wire::SurfaceValue], guest: &Guest) -> wire::Node {
    let path = surface_str(args, 0);
    let Some(bytes) = artifact_asset(&guest.assets, &path) else {
        return wire::Node::empty();
    };
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let hash = u64::from_le_bytes(digest[..8].try_into().expect("hash prefix"));
    match name {
        "artifact_svg" => wire::Node::Svg {
            key: path,
            hash,
            bytes: Some(bytes.to_vec()),
            inherit_button_ink: false,
            label: None,
            color: None,
            hover: None,
            fit: None,
            rotation: None,
            opacity: None,
            width: Some(wire::Length::Fill),
            height: Some(wire::Length::Fill),
        },
        _ => wire::Node::Image {
            key: path,
            hash,
            data: Some(wire::ImageData::Encoded(bytes.to_vec())),
            label: None,
            fit: None,
            rotation: None,
            opacity: None,
            filter: Default::default(),
            width: Some(wire::Length::Fill),
            height: Some(wire::Length::Fill),
        },
    }
}

impl NativeModuleView {
    pub(super) fn sync_surfaces(
        &mut self,
        guest: &Guest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(content) = self.content.clone() else {
            return Ok(());
        };
        let requests = content.read(cx).surface_requests();
        self.surfaces
            .retain(|key, _| requests.iter().any(|request| &request.0 == key));
        for (key, name, args, handler) in requests {
            if !surface_allowed(self.module, &name) {
                continue;
            }
            let same_kind = self
                .surfaces
                .get(&key)
                .is_some_and(|surface| surface.name == name);
            if !same_kind {
                let view =
                    match name.as_str() {
                        "chat_composer" | "forge_composer" => {
                            crate::composer_surface::validate_args(&args)?;
                            View::Composer(cx.new(|cx| {
                                crate::composer_surface::ComposerView::new(&args, window, cx)
                                    .expect("validated composer arguments")
                            }))
                        }
                        "agent_markdown" | "forge_markdown" => {
                            let (source, doc, dark) = markdown_args(&name, &args);
                            View::Markdown(cx.new(|cx| {
                                crate::backend::MarkdownSurface::new(source, doc, dark, cx)
                            }))
                        }
                        "picture" => View::Picture(cx.new(|_| {
                            crate::backend::PictureView::new(
                                surface_str(&args, 0),
                                surface_str(&args, 1),
                            )
                        })),
                        "forge_code" => View::Code(cx.new(|cx| {
                            crate::backend::CodeView::new(
                                surface_str(&args, 0),
                                surface_str(&args, 1),
                                surface_bool(&args, 2),
                                window,
                                cx,
                            )
                        })),
                        "artifact_svg" | "artifact_image" => {
                            let root = asset_node(&name, &args, guest);
                            View::Asset(cx.new(|_| crate::view_tree::ViewTree::new(root)))
                        }
                        _ => continue,
                    };
                let events = self.surface_subscription(&view, &key, handler, cx);
                let observations = self.surface_observations(&view, &key, cx);
                content.update(cx, |tree, cx| tree.set_surface(key.clone(), view.any(), cx));
                self.surfaces.insert(
                    key,
                    Surface {
                        name,
                        args,
                        handler,
                        view,
                        _events: events,
                        _observations: observations,
                    },
                );
                continue;
            }
            // Temporarily take the entry so replacing its subscription cannot
            // borrow the mounted-surface map across an entity update.
            let mut surface = self.surfaces.remove(&key).expect("existing surface");
            if surface.args != args {
                surface.view.replace(&name, &args, guest, window, cx)?;
                surface.args = args;
            }
            if surface.handler != handler {
                surface._events = self.surface_subscription(&surface.view, &key, handler, cx);
                surface.handler = handler;
            }
            self.surfaces.insert(key, surface);
        }
        Ok(())
    }

    fn surface_subscription(
        &self,
        view: &View,
        key: &str,
        handler: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        match view {
            View::Composer(view) => Some(self.subscribe_surface(view, key, handler, cx)),
            View::Markdown(view) => Some(self.subscribe_surface(view, key, handler, cx)),
            View::Picture(_) | View::Code(_) | View::Asset(_) => None,
        }
    }

    fn surface_observations(
        &self,
        view: &View,
        key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        let View::Composer(view) = view else {
            return None;
        };
        let seat = mounted(self.module);
        let generation = self.generation;
        let alive = self.alive.clone().expect("mounted guest");
        let key = key.to_owned();
        Some(cx.subscribe(view, move |this, _, event: &wire::Event, cx| {
            if !this.surfaces.contains_key(&key) {
                return;
            }
            let mut locked = seat.lock().expect("module view lock");
            let Slot::Ready(guest) = &mut locked.slot else {
                return;
            };
            if guest.seated_generation() != generation
                || !Arc::ptr_eq(&alive, &guest.alive)
                || guest.frame_rev != this.revision
            {
                cx.notify();
                return;
            }
            if input::deliver(guest, event.clone()) {
                cx.notify();
            }
        }))
    }

    fn subscribe_surface<V: EventEmitter<wire::SurfaceValue> + 'static>(
        &self,
        view: &Entity<V>,
        key: &str,
        handler: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let seat = mounted(self.module);
        let generation = self.generation;
        let alive = self.alive.clone().expect("mounted guest");
        let key = key.to_owned();
        cx.subscribe(view, move |this, _, value, cx| {
            let current_route = this
                .surfaces
                .get(&key)
                .is_some_and(|surface| surface.handler == handler);
            if !current_route {
                return;
            }
            let mut locked = seat.lock().expect("module view lock");
            let Slot::Ready(guest) = &mut locked.slot else {
                return;
            };
            let current_instance =
                guest.seated_generation() == generation && Arc::ptr_eq(&alive, &guest.alive);
            if !current_instance {
                return;
            }
            let current_frame = guest.frame_rev == this.revision;
            if !current_frame {
                cx.notify();
                return;
            }
            guest.surface_event(handler, value.clone());
            cx.notify();
        })
    }
}
