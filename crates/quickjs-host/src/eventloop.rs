//! The event loop: drain microtasks, wait for the nearest deadline, run one timer, repeat.
//!
//! The shape is `js_std_loop`'s, split in two. The engine owns the *logic* — what is due,
//! what order to run it in — and this module owns the *waiting*, because the engine has no
//! dependencies and cannot know whether "wait 10ms" means `thread::sleep`, a tokio timer or
//! a test clock that jumps forward instantly.
//!
//! ```text
//! loop {
//!     drain microtasks                 // engine: Interp::run_one_turn
//!     if a timer is due { run it }     // engine
//!     else if nothing pending { break }// engine: Turn::Done
//!     else { sleep until it is due }   // host: this module
//! }
//! ```

use quickjs::{Context, Turn};

/// Why the loop stopped.
#[derive(Debug)]
pub enum RunError {
    /// The script raised. The loop stops, because a failure outside a callback is fatal
    /// and continuing would run later work against a broken state.
    Script(quickjs::Error),
    /// A timer callback threw. Recorded rather than fatal, matching `js_std_loop`, which
    /// reports the error and keeps running so one bad timer cannot stop the rest.
    TimerCallback(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Script(error) => write!(f, "{error}"),
            RunError::TimerCallback(text) => write!(f, "{text}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Run the event loop until nothing is pending, returning the errors timer callbacks threw.
///
/// Blocks the calling thread. A caller that needs to tick something else — a REPL reading
/// stdin, say — wants [`run_once`] instead.
///
/// # Bounded, deliberately
///
/// A self-rescheduling `setTimeout(f, 0)` keeps the loop pending forever, and so does a
/// `setInterval` nobody clears. That is a script bug, but a host that hangs is worse than a
/// host that reports one, so the number of turns is capped: at the cap the loop returns
/// what it has and names the problem, rather than spinning until the process is killed.
pub fn run_until_idle(ctx: &mut Context<'_>) -> Result<Vec<String>, RunError> {
    // Generous enough that no legitimate script reaches it, small enough to notice.
    const MAX_TURNS: usize = 1_000_000;
    let mut turns = 0usize;
    loop {
        turns += 1;
        if turns > MAX_TURNS {
            return Err(RunError::TimerCallback(format!(
                "event loop did not go idle after {MAX_TURNS} turns: a timer keeps \
                 rescheduling itself (`setTimeout(f, 0)` or an uncleared `setInterval`)"
            )));
        }
        // Signals first: the handler has to run before the loop decides it is idle, or a
        // SIGINT that arrived during the last turn would be dropped at exit.
        serve_signals(ctx)?;
        match ctx.run_one_turn().map_err(RunError::Script)? {
            Turn::Ran => {}
            Turn::Done => break,
            Turn::Wait(delay) => sleep(delay),
            Turn::Poll(timeout) => poll_and_fire(ctx, timeout)?,
        }
    }
    // Collected after the loop: a callback that throws does not stop its successors, so
    // there can be several, and the host reports them all rather than the first.
    let mut errors = ctx.take_timer_errors();
    errors.extend(ctx.take_unhandled_rejections());
    Ok(errors)
}

/// Run turns until either nothing is runnable right now or `budget` runs out.
///
/// For a host that has other work to do — the REPL has to notice stdin becoming readable
/// while a script's timers are pending. Returns `Ok(false)` when the loop is finished, so
/// the caller can stop calling it.
pub fn run_once(ctx: &mut Context<'_>, budget: usize) -> Result<bool, RunError> {
    for _ in 0..budget {
        serve_signals(ctx)?;
        match ctx.run_one_turn().map_err(RunError::Script)? {
            Turn::Ran => {}
            Turn::Done => return Ok(false),
            // A descriptor is registered. Serving it here rather than returning keeps
            // `run_once` from reporting "idle" while a handler is waiting to fire.
            Turn::Poll(timeout) => poll_and_fire(ctx, timeout)?,
            // Nothing due yet; the caller decides how to wait, so stop rather than sleep.
            Turn::Wait(_) => return Ok(true),
        }
    }
    Ok(true)
}

/// Run the JavaScript handler for every signal that arrived since the last call.
///
/// The kernel-side handler only sets a bit (see [`crate::signal`]); this is where the bit
/// turns into a call, on the loop's thread rather than in signal context, which is what makes
/// allocating and running script code safe.
fn serve_signals(ctx: &mut Context<'_>) -> Result<(), RunError> {
    for signal in crate::signal::take_pending() {
        ctx.fire_signal(signal).map_err(RunError::Script)?;
    }
    Ok(())
}

/// Wait on the registered descriptors for at most `timeout`, then run a ready handler.
///
/// This is the host half of `os.setReadHandler`: the engine knows *which* descriptors have
/// handlers but cannot call `poll(2)`, so it reports `Turn::Poll` and the host does the
/// waiting. `poll(2)` with no descriptors left is a plain timed sleep, which is why the
/// fallback is `sleep` rather than an error.
fn poll_and_fire(ctx: &mut Context<'_>, timeout: std::time::Duration) -> Result<(), RunError> {
    let interest = ctx.pending_fds();
    if interest.is_empty() {
        sleep(timeout);
        return Ok(());
    }
    let mut watched: Vec<crate::fds::Watched> = Vec::with_capacity(interest.len());
    for entry in &interest {
        watched.push(crate::fds::Watched {
            fd: entry.fd,
            read: entry.read,
            write: entry.write,
        });
    }
    // The table is the thread's, registered when `os`/`std` were installed; without it there
    // is nothing to borrow and the descriptors cannot be polled.
    let ready = crate::fdtable::current(|table| match table {
        Some(table) => crate::fds::poll(table, &watched, timeout),
        None => vec![crate::fds::Ready::default(); watched.len()],
    });
    // One handler per iteration, matching the reference's `goto done`: the handler may have
    // re-registered descriptors, so a second one would use stale state.
    for (index, event) in ready.iter().enumerate() {
        if event.read || event.write {
            let fd = watched[index].fd;
            ctx.fire_fd(fd, event.read, event.write)
                .map_err(RunError::Script)?;
            return Ok(());
        }
    }
    Ok(())
}

/// Block for `delay`.
///
/// `thread::sleep` rather than a tokio timer: the loop is synchronous — it calls into the
/// engine between turns and the engine is not `async` — so there is no reactor to await on
/// at this point. This is the same thing the reference does when `poll(2)` has only a
/// timeout and no descriptors.
fn sleep(delay: std::time::Duration) {
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickjs::Runtime;

    /// A context with the timer functions installed on a global `os`, the way the reference
    /// exposes them — `os.setTimeout`, not a bare `setTimeout`.
    fn context_with_timers<'rt>(runtime: &'rt Runtime) -> Context<'rt> {
        let mut ctx = Context::new(runtime);
        let os = ctx.new_object();
        crate::timers::install(&mut ctx, &os).expect("timers install");
        ctx.set_global("os", os).expect("os installed");
        ctx
    }

    /// A timer fires after the loop waits, and its output is visible in order.
    #[test]
    fn a_timer_runs_after_the_script_body() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval(
            r#"
            print("a");
            os.setTimeout(function () { print("c"); }, 5);
            print("b");
            "#,
        )
        .expect("script runs");
        // Before the loop runs, the timer has not fired.
        assert_eq!(ctx.output(), "a\nb\n");
        run_until_idle(&mut ctx).expect("loop finishes");
        assert_eq!(ctx.output(), "a\nb\nc\n");
    }

    /// Timers run in deadline order, not registration order — the property a naive
    /// "first in the queue" loop gets wrong.
    #[test]
    fn timers_run_in_deadline_order() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval(
            r#"
            os.setTimeout(function () { print("late"); }, 40);
            os.setTimeout(function () { print("early"); }, 1);
            os.setTimeout(function () { print("middle"); }, 20);
            "#,
        )
        .expect("script schedules");
        run_until_idle(&mut ctx).expect("loop finishes");
        assert_eq!(ctx.output(), "early\nmiddle\nlate\n");
    }

    /// `clearTimeout` cancels, and an id that never existed is a no-op rather than a panic.
    #[test]
    fn clear_timeout_cancels() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval(
            r#"
            var id = os.setTimeout(function () { print("should not run"); }, 1);
            os.clearTimeout(id);
            os.clearTimeout(99999);
            os.clearTimeout(id);
            os.setTimeout(function () { print("runs"); }, 1);
            "#,
        )
        .expect("script schedules");
        run_until_idle(&mut ctx).expect("loop finishes");
        assert_eq!(ctx.output(), "runs\n");
    }

    /// A throwing timer callback is recorded and does not stop later timers.
    #[test]
    fn a_throwing_timer_does_not_stop_the_loop() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval(
            r#"
            os.setTimeout(function () { throw new Error("boom"); }, 1);
            os.setTimeout(function () { print("still here"); }, 2);
            "#,
        )
        .expect("script schedules");
        let errors = run_until_idle(&mut ctx).expect("loop finishes");
        assert_eq!(ctx.output(), "still here\n");
        assert_eq!(errors.len(), 1, "the throw is reported: {errors:?}");
        assert!(errors[0].contains("boom"), "{errors:?}");
    }

    /// Microtasks queued inside a timer run before the next timer, which is the ordering
    /// that makes `await` inside a timer callback behave.
    #[test]
    fn microtasks_drain_between_timers() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval(
            r#"
            os.setTimeout(function () {
                print("timer1");
                Promise.resolve().then(function () { print("micro"); });
            }, 1);
            os.setTimeout(function () { print("timer2"); }, 2);
            "#,
        )
        .expect("script schedules");
        run_until_idle(&mut ctx).expect("loop finishes");
        assert_eq!(ctx.output(), "timer1\nmicro\ntimer2\n");
    }

    /// A loop with nothing pending finishes immediately rather than waiting.
    #[test]
    fn an_idle_loop_returns_at_once() {
        let runtime = Runtime::new();
        let mut ctx = context_with_timers(&runtime);
        ctx.eval("1 + 1").expect("script runs");
        let started = std::time::Instant::now();
        run_until_idle(&mut ctx).expect("loop finishes");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "an idle loop must not sleep"
        );
    }
}
