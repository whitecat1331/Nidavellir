use agent::{AnchorDescriptor, DbReviewComment, ReviewCommentStatus};
use agent_client_protocol::schema::v1 as acp;
use chrono::Utc;
use gpui::{
    App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Subscription, Window, prelude::*,
};
use ui::{LabelSize, prelude::*};
use ui_input::InputField;
use workspace::ModalView;

/// Emitted whenever the in-memory comment set changes, so the review pane can
/// refresh its hunk annotations.
pub enum ReviewCommentsEvent {
    Changed,
}

/// The review comments attached to one agent thread.
///
/// Comments are loaded from the threads database and cached in memory for the
/// diff pane. Each carries an [`AnchorDescriptor`] rather than a live anchor, so
/// a comment survives edits; the live position is re-derived against the current
/// buffer when it is read back (see [`AnchorDescriptor::resolve_offset`]).
pub struct ReviewComments {
    session_id: acp::SessionId,
    comments: Vec<DbReviewComment>,
}

impl EventEmitter<ReviewCommentsEvent> for ReviewComments {}

impl ReviewComments {
    pub fn new(session_id: acp::SessionId, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                session_id: session_id.clone(),
                comments: Vec::new(),
            };
            this.reload(cx);
            this
        })
    }

    pub fn comments(&self) -> &[DbReviewComment] {
        &self.comments
    }

    /// Number of non-resolved comments attached to `file_path`.
    pub fn open_count_for_file(&self, file_path: &str) -> usize {
        self.comments
            .iter()
            .filter(|comment| {
                comment.anchor.file_path == file_path
                    && comment.status != ReviewCommentStatus::Resolved
            })
            .count()
    }

    /// Reload the comment set from the database.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let load = agent::load_review_comments(cx, self.session_id.clone());
        cx.spawn(async move |this, cx| {
            let Ok(comments) = load.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.comments = comments;
                cx.emit(ReviewCommentsEvent::Changed);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Persist a new comment anchored to `anchor` and add it to the cache.
    pub fn add(&mut self, anchor: AnchorDescriptor, body: String, cx: &mut Context<Self>) {
        let now = Utc::now();
        let comment = DbReviewComment {
            id: uuid::Uuid::new_v4().to_string(),
            thread_id: self.session_id.clone(),
            anchor,
            body,
            status: ReviewCommentStatus::Open,
            created_at: now,
            updated_at: now,
        };
        self.comments.push(comment.clone());
        cx.emit(ReviewCommentsEvent::Changed);
        cx.notify();
        persist(cx, comment);
    }

    /// Remove a comment from the cache and the database.
    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        self.comments.retain(|comment| comment.id != id);
        cx.emit(ReviewCommentsEvent::Changed);
        cx.notify();

        let delete = agent::delete_review_comment(cx, id.to_string());
        cx.spawn(async move |_this, _cx| {
            if let Err(error) = delete.await {
                log::error!("[REVIEW] failed to delete review comment: {error:#}");
            }
        })
        .detach();
    }
}

fn persist(cx: &mut Context<ReviewComments>, comment: DbReviewComment) {
    let save = agent::save_review_comment(cx, comment);
    cx.spawn(async move |_this, _cx| {
        if let Err(error) = save.await {
            log::error!("[REVIEW] failed to save review comment: {error:#}");
        }
    })
    .detach();
}

/// A modal for adding a review comment anchored to one diff hunk, and for
/// reviewing the comments already attached to that file.
pub struct ReviewCommentModal {
    comments: Entity<ReviewComments>,
    descriptor: AnchorDescriptor,
    editor: Entity<InputField>,
    _subscription: Subscription,
}

impl Focusable for ReviewCommentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl ModalView for ReviewCommentModal {}
impl EventEmitter<DismissEvent> for ReviewCommentModal {}

impl ReviewCommentModal {
    pub fn new(
        comments: Entity<ReviewComments>,
        descriptor: AnchorDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx
            .new(|cx| InputField::new(window, cx, "Leave a review comment…").label("New comment"));
        let subscription = cx.observe_in(&comments, window, |_this, _comments, _window, cx| {
            cx.notify();
        });

        Self {
            comments,
            descriptor,
            editor,
            _subscription: subscription,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let anchor = self.descriptor.clone();
        self.comments
            .update(cx, |comments, cx| comments.add(anchor, body, cx));
        self.editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        cx.emit(DismissEvent);
    }
}

impl Render for ReviewCommentModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let file_path = self.descriptor.file_path.clone();
        let comments = self
            .comments
            .read(cx)
            .comments()
            .iter()
            .filter(|comment| comment.anchor.file_path == file_path)
            .cloned()
            .enumerate()
            .map(|(index, comment)| {
                let id = comment.id.clone();
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(comment.body))
                    .child(h_flex().justify_end().child(
                        Button::new(("delete-review-comment", index), "Delete").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.comments
                                    .update(cx, |comments, cx| comments.remove(&id, cx));
                            }),
                        ),
                    ))
            })
            .collect::<Vec<_>>();

        v_flex()
            .key_context("ReviewCommentModal")
            .elevation_3(cx)
            .bg(cx.theme().colors().editor_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .w(gpui::px(420.))
            .max_h(gpui::px(480.))
            .gap_2()
            .p_3()
            .child(Label::new("Review comments"))
            .child(Label::new(file_path).size(LabelSize::Small))
            .children(comments)
            .child(self.editor.clone())
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("submit-review-comment", "Comment")
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    )
                    .child(
                        Button::new("close-review-comment", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
    }
}
