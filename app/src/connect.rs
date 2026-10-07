use std::{sync::Arc, time::Duration};

use ely_gpui_component::{
    buttons::SegmentedControl,
    forms::{Choice, Input, InputEvent, PasswordInput, Select, TextInput},
    theme::{ActiveTheme, ControlSize},
};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use shelf_core::{
    BucketConfig,
    cloudflare::{self, Account, CfError, Session},
};

use crate::{
    Rt,
    ui::{self, PillKind, StepState},
};

/// Credentials that listed the bucket and are now saved (secret in the Keychain).
pub struct Connected(pub BucketConfig, pub String);

enum Status {
    Idle,
    Busy,
    Ok,
    Failed(SharedString),
}

/// The Cloudflare one-click flow: which step it is on and what that step holds.
enum Cf {
    /// Step 1 waits for a click (or shows why the last try failed).
    Start(Option<SharedString>),
    /// Step 1: the browser is open.
    Login,
    /// Step 2: fetching accounts.
    Accounts(Arc<Session>),
    /// Step 2: several accounts, `usize` is the highlighted one.
    Pick(Arc<Session>, Vec<Account>, usize),
    /// Step 3: creating (`None`) or failed.
    Create(Arc<Session>, Account, Option<Fail>),
    /// Account name, bucket name.
    Done(SharedString, SharedString),
}

enum Fail {
    Text(SharedString),
    /// R2 isn't enabled yet; the URL is the dashboard page that turns it on.
    R2(String),
}

fn cf_text(error: CfError) -> SharedString {
    match error {
        CfError::Other(error) => shelf_core::friendly(&error).into(),
        error => error.to_string().into(),
    }
}

/// Drop the token Cloudflare gave us; best effort, and only if nothing else still holds the session.
fn revoke(session: Arc<Session>, cx: &App) {
    cx.global::<Rt>().0.spawn(async move {
        if let Ok(session) = Arc::try_unwrap(session) {
            session.revoke().await.ok();
        }
    });
}

pub struct Connect {
    cf: Cf,
    manual: bool,
    /// Bumped whenever the Cloudflare flow is cancelled or restarted, so late results are ignored.
    epoch: u64,
    login_abort: Option<tokio::task::AbortHandle>,
    provider: SharedString,
    target: Entity<TextInput>,
    bucket: Entity<TextInput>,
    key_id: Entity<TextInput>,
    secret: Entity<TextInput>,
    status: Status,
    _subs: Vec<Subscription>,
}

impl EventEmitter<Connected> for Connect {}

impl Connect {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut field = |hint: &'static str, masked: bool| {
            cx.new(|cx| {
                let input = TextInput::new(window, cx).placeholder(hint);
                if masked { input.masked() } else { input }
            })
        };
        let inputs = [
            field("Account ID", false),
            field("my-bucket", false),
            field("Access Key ID", false),
            field("Secret Access Key", true),
        ];
        // Editing clears an old result; Enter in any field connects.
        let _subs = inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| match event {
                    InputEvent::Submit => this.run(cx),
                    InputEvent::Changed => {
                        if matches!(this.status, Status::Failed(_)) {
                            this.status = Status::Idle;
                        }
                        cx.notify();
                    }
                    _ => {}
                })
            })
            .collect();
        let [target, bucket, key_id, secret] = inputs;
        Self {
            cf: Cf::Start(None),
            manual: cloudflare::client_id().is_none(),
            epoch: 0,
            login_abort: None,
            provider: "r2".into(),
            target,
            bucket,
            key_id,
            secret,
            status: Status::Idle,
            _subs,
        }
    }

    /// Step 1: open the browser and wait for the sign-in to finish.
    fn sign_in(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Cf::Start(_)) = (cloudflare::client_id(), &self.cf) else { return };
        self.epoch += 1;
        let epoch = self.epoch;
        self.cf = Cf::Login;
        let job = cx.global::<Rt>().0.spawn(cloudflare::login(id));
        self.login_abort = Some(job.abort_handle());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.epoch != epoch {
                    return;
                }
                this.login_abort = None;
                match result {
                    Ok(Ok(session)) => this.list_accounts(Arc::new(session), cx),
                    Ok(Err(CfError::Cancelled)) | Err(_) => this.cf = Cf::Start(None),
                    Ok(Err(error)) => this.cf = Cf::Start(Some(cf_text(error))),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Step 2: one account carries on by itself, several wait for a pick.
    fn list_accounts(&mut self, session: Arc<Session>, cx: &mut Context<Self>) {
        let epoch = self.epoch;
        self.cf = Cf::Accounts(session.clone());
        let held = session.clone();
        let job = cx.global::<Rt>().0.spawn(async move {
            let accounts = held.accounts().await;
            drop(held);
            accounts
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.epoch != epoch {
                    return;
                }
                let failed = |this: &mut Self, text: SharedString, session| {
                    this.cf = Cf::Start(Some(text));
                    revoke(session, cx);
                };
                match result {
                    Ok(Ok(mut accounts)) if accounts.len() == 1 => this.create(session, accounts.remove(0), cx),
                    Ok(Ok(accounts)) if !accounts.is_empty() => this.cf = Cf::Pick(session, accounts, 0),
                    Ok(Ok(_)) => failed(this, "No Cloudflare account found".into(), session),
                    Ok(Err(error)) => failed(this, cf_text(error), session),
                    Err(error) => failed(this, shelf_core::friendly(&error.into()).into(), session),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Step 3: make the bucket and its key, save them like the manual path, and move on.
    fn create(&mut self, session: Arc<Session>, account: Account, cx: &mut Context<Self>) {
        self.epoch += 1;
        let epoch = self.epoch;
        self.cf = Cf::Create(session.clone(), account.clone(), None);
        cx.notify();
        let (held, target) = (session.clone(), account.clone());
        let job = cx.global::<Rt>().0.spawn(async move {
            let made = held.provision(&target).await;
            drop(held);
            made
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let done = this
                .update(cx, |this, cx| {
                    if this.epoch != epoch {
                        return None;
                    }
                    let made = match result {
                        Ok(Ok((cfg, secret))) => shelf_core::save_secret(&cfg.bucket, &secret)
                            .and_then(|()| shelf_core::save_config(&cfg))
                            .map(|()| (cfg, secret))
                            .map_err(|e| Fail::Text(shelf_core::friendly(&e).into())),
                        Ok(Err(CfError::R2NotEnabled { url })) => Err(Fail::R2(url)),
                        Ok(Err(error)) => Err(Fail::Text(cf_text(error))),
                        Err(error) => Err(Fail::Text(shelf_core::friendly(&error.into()).into())),
                    };
                    let done = match made {
                        Ok((cfg, secret)) => {
                            this.cf = Cf::Done(account.name.into(), cfg.bucket.clone().into());
                            revoke(session, cx);
                            Some((cfg, secret))
                        }
                        Err(fail) => {
                            this.cf = Cf::Create(session, account, Some(fail));
                            None
                        }
                    };
                    cx.notify();
                    done
                })
                .ok()
                .flatten();
            // Same hold as the manual path, then hand over.
            if let Some((cfg, secret)) = done {
                cx.background_executor().timer(Duration::from_millis(600)).await;
                this.update(cx, |_, cx| cx.emit(Connected(cfg, secret))).ok();
            }
        })
        .detach();
    }

    /// Back to step 1: stops a pending sign-in, ignores late results and drops any token we hold.
    fn reset_cf(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        if let Some(abort) = self.login_abort.take() {
            abort.abort();
        }
        match std::mem::replace(&mut self.cf, Cf::Start(None)) {
            Cf::Accounts(session) | Cf::Pick(session, ..) | Cf::Create(session, ..) => revoke(session, cx),
            _ => {}
        }
        cx.notify();
    }

    /// Whether the "instead" link is safe to show (nothing half-made to abandon).
    fn can_switch(&self) -> bool {
        if self.manual {
            matches!(self.status, Status::Idle | Status::Failed(_))
        } else {
            !matches!(self.cf, Cf::Accounts(_) | Cf::Done(..) | Cf::Create(_, _, None))
        }
    }

    fn filled(&self, cx: &App) -> bool {
        [&self.target, &self.bucket, &self.key_id, &self.secret]
            .iter()
            .all(|input| !input.read(cx).text().trim().is_empty())
    }

    /// Row 1 is always done (R2 is the default); row 2 waits on the fields; row 3 follows `status`.
    fn step_states(&self, cx: &App) -> [StepState; 3] {
        let filled = self.filled(cx);
        let third = match &self.status {
            Status::Busy => StepState::Busy,
            Status::Ok => StepState::Done,
            Status::Failed(_) => StepState::Failed,
            Status::Idle if filled => StepState::Active(3),
            Status::Idle => StepState::Pending(3),
        };
        [StepState::Done, if filled { StepState::Done } else { StepState::Active(2) }, third]
    }

    /// Lists the bucket to prove the credentials work, then stores them and moves on.
    fn run(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Busy) || !self.filled(cx) {
            return;
        }
        let text = |input: &Entity<TextInput>| input.read(cx).text().trim().to_string();
        let (target, bucket, key_id, secret) =
            (text(&self.target), text(&self.bucket), text(&self.key_id), text(&self.secret));
        let endpoint = match &*self.provider {
            "r2" => format!("https://{target}.r2.cloudflarestorage.com"),
            _ => target.trim_end_matches('/').to_string(),
        };
        let cfg = BucketConfig::new(endpoint, bucket, key_id);
        let store = match shelf_core::open_quick(&cfg, &secret) {
            Ok(store) => store,
            Err(error) => {
                self.status = Status::Failed(shelf_core::friendly(&error).into());
                return cx.notify();
            }
        };
        self.status = Status::Busy;
        cx.notify();
        let job = cx
            .global::<Rt>()
            .0
            .spawn(async move { shelf_core::list_dir(&*store, "").await });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let done = this.update(cx, |this, cx| {
                let result = match result {
                    Ok(listed) => listed.map_err(|e| shelf_core::friendly(&e)),
                    Err(error) => Err(shelf_core::friendly(&error.into())),
                };
                let saved = result.and_then(|_| {
                    shelf_core::save_secret(&cfg.bucket, &secret)
                        .and_then(|()| shelf_core::save_config(&cfg))
                        .map_err(|e| shelf_core::friendly(&e))
                });
                let ok = saved.is_ok();
                this.status = match saved {
                    Ok(()) => Status::Ok,
                    Err(error) => Status::Failed(error.into()),
                };
                cx.notify();
                ok.then_some((cfg, secret))
            })
            .ok()
            .flatten();
            // Let the green check land before the window switches.
            if let Some((cfg, secret)) = done {
                cx.background_executor().timer(Duration::from_millis(600)).await;
                this.update(cx, |_, cx| cx.emit(Connected(cfg, secret))).ok();
            }
        })
        .detach();
    }
}

impl Connect {
    fn cf_rows(&self, cx: &App, this: &Entity<Self>) -> Vec<AnyElement> {
        let colors = &cx.theme().colors;
        let (muted, fg) = (colors.fg_muted, colors.fg);
        let name = match &self.cf {
            Cf::Create(_, account, _) => Some(SharedString::from(account.name.clone())),
            Cf::Done(name, _) => Some(name.clone()),
            _ => None,
        };
        let [s1, s2, s3] = match &self.cf {
            Cf::Start(None) => [StepState::Active(1), StepState::Pending(2), StepState::Pending(3)],
            Cf::Start(Some(_)) => [StepState::Failed, StepState::Pending(2), StepState::Pending(3)],
            Cf::Login => [StepState::Busy, StepState::Pending(2), StepState::Pending(3)],
            Cf::Accounts(_) => [StepState::Done, StepState::Busy, StepState::Pending(3)],
            Cf::Pick(..) => [StepState::Done, StepState::Active(2), StepState::Pending(3)],
            Cf::Create(_, _, None) => [StepState::Done, StepState::Done, StepState::Busy],
            Cf::Create(_, _, Some(_)) => [StepState::Done, StepState::Done, StepState::Failed],
            Cf::Done(..) => [StepState::Done; 3],
        };

        // Step 1.
        let detail1: SharedString = match &self.cf {
            Cf::Start(None) => "Opens Cloudflare in your browser.".into(),
            Cf::Start(Some(error)) => error.clone(),
            Cf::Login => "Finish in your browser\u{2026}".into(),
            _ => name.clone().unwrap_or_else(|| "Signed in".into()),
        };
        let trailing1 = match &self.cf {
            Cf::Start(_) => {
                let go = this.clone();
                Some(
                    ui::pill_button("sign-in", "Sign in", PillKind::Accent, true, false, cx)
                        .on_click(move |_, _, cx| go.update(cx, |this, cx| this.sign_in(cx)))
                        .into_any_element(),
                )
            }
            Cf::Login => {
                let stop = this.clone();
                Some(
                    div()
                        .id("cancel")
                        .text_size(px(12.0))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(move |s| s.text_color(fg))
                        .child("Cancel")
                        .on_click(move |_, _, cx| stop.update(cx, |this, cx| this.reset_cf(cx)))
                        .into_any_element(),
                )
            }
            _ => None,
        };

        // Step 2.
        let detail2: Option<SharedString> = match &self.cf {
            Cf::Accounts(_) => Some("Looking up your accounts\u{2026}".into()),
            Cf::Create(..) | Cf::Done(..) => name,
            _ => None,
        };
        let trailing2 = match &self.cf {
            Cf::Pick(_, accounts, chosen) => {
                let (pick, go) = (this.clone(), this.clone());
                let choices = accounts.iter().map(|a| Choice::new(a.id.clone(), a.name.clone()));
                let select = Select::new("account", choices)
                    .selected(accounts[*chosen].id.clone())
                    .size(ControlSize::Sm)
                    .on_change(move |value, _, cx| {
                        pick.update(cx, |this, cx| {
                            if let Cf::Pick(_, accounts, chosen) = &mut this.cf {
                                *chosen = accounts.iter().position(|a| a.id == **value).unwrap_or(*chosen);
                            }
                            cx.notify();
                        })
                    });
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(130.0)).child(select))
                        .child(
                            ui::pill_button("continue", "Continue", PillKind::Accent, true, false, cx).on_click(
                                move |_, _, cx| {
                                    go.update(cx, |this, cx| {
                                        if let Cf::Pick(session, accounts, chosen) = &this.cf {
                                            let (session, account) = (session.clone(), accounts[*chosen].clone());
                                            this.create(session, account, cx);
                                        }
                                    })
                                },
                            ),
                        )
                        .into_any_element(),
                )
            }
            _ => None,
        };

        // Step 3.
        let detail3: SharedString = match &self.cf {
            Cf::Create(_, _, None) => "Creating bucket and a key that can only access it\u{2026}".into(),
            Cf::Create(_, _, Some(Fail::Text(text))) => text.clone(),
            Cf::Create(_, _, Some(Fail::R2(_))) => "Turn on R2 in your Cloudflare dashboard (one time)".into(),
            Cf::Done(_, bucket) => format!("{bucket} is ready").into(),
            _ => "A private bucket just for Shelf.".into(),
        };
        let trailing3 = if let Cf::Create(_, _, Some(fail)) = &self.cf {
            let again = this.clone();
            let retry = ui::pill_button("retry", "Try again", PillKind::Neutral, true, false, cx).on_click(
                move |_, _, cx| {
                    again.update(cx, |this, cx| {
                        if let Cf::Create(session, account, Some(_)) = &this.cf {
                            let (session, account) = (session.clone(), account.clone());
                            this.create(session, account, cx);
                        }
                    })
                },
            );
            let mut buttons = div().flex().items_center().gap(px(6.0));
            if let Fail::R2(url) = fail {
                let url = url.clone();
                buttons = buttons.child(
                    ui::pill_button("dashboard", "Open dashboard", PillKind::Accent, true, false, cx)
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            Some(buttons.child(retry).into_any_element())
        } else {
            None
        };

        vec![
            ui::step_row("c1", s1, "Sign in with Cloudflare", Some(detail1), trailing1, None, cx),
            ui::step_row("c2", s2, "Choose account", detail2, trailing2, None, cx),
            ui::step_row("c3", s3, "Create your bucket", Some(detail3), trailing3, None, cx),
        ]
    }
}

impl Render for Connect {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        if !self.manual {
            let rows = self.cf_rows(cx, &this);
            return self.screen(rows, &this, cx);
        }
        let [s1, s2, s3] = self.step_states(cx);
        let (busy, done) = (s3 == StepState::Busy, s3 == StepState::Done);
        let text = |input: &Entity<TextInput>| input.read(cx).text().trim().to_string();

        let picked = this.clone();
        let provider = SegmentedControl::new("provider", self.provider.clone())
            .segment("r2", "R2", None)
            .segment("custom", "S3-compatible", None)
            .size(ControlSize::Sm)
            .on_change(move |value, _, cx| {
                let value = value.clone();
                picked.update(cx, |this, cx| {
                    let hint = if &*value == "r2" { "Account ID" } else { "https://s3.example.com" };
                    this.provider = value;
                    this.target.update(cx, |input, cx| input.set_placeholder(hint, cx));
                    cx.notify();
                });
            });

        let fields = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .pl(px(52.0))
            .pr(px(14.0))
            .pb(px(12.0))
            .child(Input::new(&self.target))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(div().flex_1().child(Input::new(&self.bucket)))
                    .child(div().flex_1().child(Input::new(&self.key_id))),
            )
            .child(PasswordInput::new(&self.secret));

        let detail = match &self.status {
            Status::Failed(error) => error.clone(),
            Status::Busy => "Checking your bucket...".into(),
            Status::Ok => "Connected.".into(),
            Status::Idle => "The secret stays in your Keychain.".into(),
        };
        let go = this.clone();
        let button = ui::pill_button("go", "Connect", PillKind::Accent, s3 == StepState::Active(3), busy, cx)
            .on_click(move |_, _, cx| go.update(cx, |this, cx| this.run(cx)));
        let summary = format!("{} \u{b7} {}", text(&self.bucket), text(&self.key_id));

        let rows = vec![
            ui::step_row("s1", s1, "Choose provider", None, Some(provider.into_any_element()), None, cx),
            ui::step_row(
                "s2",
                s2,
                "Credentials",
                done.then(|| summary.into()),
                None,
                (!done).then(|| fields.into_any_element()),
                cx,
            ),
            ui::step_row("s3", s3, "Test connection", Some(detail), Some(button.into_any_element()), None, cx),
        ];
        self.screen(rows, &this, cx)
    }
}

impl Connect {
    /// Hero, the step card and the link that swaps between the two flows.
    fn screen(&self, rows: Vec<AnyElement>, this: &Entity<Self>, cx: &App) -> gpui::Div {
        let colors = &cx.theme().colors;
        let (accent, hover) = (colors.accent, colors.accent_hover);
        let switch = (cloudflare::client_id().is_some() && self.can_switch()).then(|| {
            let flip = this.clone();
            let label = if self.manual { "Sign in with Cloudflare instead" } else { "Use access keys instead" };
            div()
                .id("switch")
                .mt(px(8.0))
                .text_size(px(12.0))
                .text_color(accent)
                .cursor_pointer()
                .hover(move |s| s.text_color(hover))
                .child(label)
                .on_click(move |_, _, cx| {
                    flip.update(cx, |this, cx| {
                        this.reset_cf(cx);
                        this.manual = !this.manual;
                        cx.notify();
                    })
                })
        });
        // The long manual card plus the link only fit if the hero rides a little higher.
        let (top, gap) = if self.manual && switch.is_some() { (0.0, 10.0) } else { (8.0, 18.0) };
        // No fill: the glass behind the window is the card's background.
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .child(div().pt(px(top)).child(ui::hero(
                "Connect your bucket",
                "Files go straight to your own bucket.",
                cx,
            )))
            .child(div().w(px(432.0)).mt(px(gap)).child(ui::group_card("setup", rows, 52.0, cx)))
            .children(switch)
    }
}
