---------------------------- MODULE InterruptReap ----------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS Workers

VARIABLES
    cli,              \* "scheduling" | "draining" | "exited"
    interrupts,       \* 0 | 1 | 2
    worker_state,     \* [w \in Workers -> "idle" | "running" | "stopping" | "reaped"]
    worker_term,      \* [w \in Workers -> "obeys" | "ignores"]
    grace_expired,    \* [w \in Workers -> BOOLEAN]
    next_cli          \* "waiting" | "acquired"

vars == <<cli, interrupts, worker_state, worker_term, grace_expired, next_cli>>

ActiveWorkers == {w \in Workers : worker_state[w] \in {"running", "stopping"}}
LockHolders == (IF cli # "exited" THEN {"cli"} ELSE {}) \union ActiveWorkers

Init ==
    /\ cli = "scheduling"
    /\ interrupts = 0
    /\ worker_state = [w \in Workers |-> "idle"]
    /\ worker_term \in [Workers -> {"obeys", "ignores"}]
    /\ grace_expired = [w \in Workers |-> FALSE]
    /\ next_cli = "waiting"

\* Start a new worker only while scheduling and before any interrupt.
StartWorker(w) ==
    /\ cli = "scheduling"
    /\ interrupts = 0
    /\ worker_state[w] = "idle"
    /\ worker_state' = [worker_state EXCEPT ![w] = "running"]
    /\ UNCHANGED <<cli, interrupts, worker_term, grace_expired, next_cli>>

\* A worker finishes naturally while running.
WorkerNaturalExit(w) ==
    /\ worker_state[w] = "running"
    /\ worker_state' = [worker_state EXCEPT ![w] = "reaped"]
    /\ UNCHANGED <<cli, interrupts, worker_term, grace_expired, next_cli>>

\* Receive first SIGINT: stop scheduling and enter draining.
FirstSigint ==
    /\ cli = "scheduling"
    /\ interrupts = 0
    /\ interrupts' = 1
    /\ cli' = "draining"
    /\ UNCHANGED <<worker_state, worker_term, grace_expired, next_cli>>

\* Receive second SIGINT while draining: escalate immediately to SIGKILL.
SecondSigint ==
    /\ cli = "draining"
    /\ interrupts = 1
    /\ interrupts' = 2
    /\ UNCHANGED <<cli, worker_state, worker_term, grace_expired, next_cli>>

\* During draining, send SIGTERM to an active running worker.
SendSigterm(w) ==
    /\ cli = "draining"
    /\ worker_state[w] = "running"
    /\ worker_state' = [worker_state EXCEPT ![w] = "stopping"]
    /\ UNCHANGED <<cli, interrupts, worker_term, grace_expired, next_cli>>

\* A worker that obeys SIGTERM exits and is reaped during the grace period.
WorkerReapedAfterSigterm(w) ==
    /\ cli = "draining"
    /\ worker_state[w] = "stopping"
    /\ worker_term[w] = "obeys"
    /\ worker_state' = [worker_state EXCEPT ![w] = "reaped"]
    /\ UNCHANGED <<cli, interrupts, worker_term, grace_expired, next_cli>>

\* Bounded grace period expires for a worker still stopping.
GraceExpires(w) ==
    /\ cli = "draining"
    /\ worker_state[w] = "stopping"
    /\ ~grace_expired[w]
    /\ grace_expired' = [grace_expired EXCEPT ![w] = TRUE]
    /\ UNCHANGED <<cli, interrupts, worker_state, worker_term, next_cli>>

\* Send SIGKILL and reap when grace expired or a second SIGINT forced immediate kill.
SendSigkillAndReap(w) ==
    /\ cli = "draining"
    /\ ( (worker_state[w] = "stopping" /\ grace_expired[w])
         \/ (worker_state[w] \in {"running", "stopping"} /\ interrupts >= 2) )
    /\ worker_state' = [worker_state EXCEPT ![w] = "reaped"]
    /\ UNCHANGED <<cli, interrupts, worker_term, grace_expired, next_cli>>

\* CLI exits only after every active worker has been reaped.
CliExit ==
    /\ cli = "draining"
    /\ ActiveWorkers = {}
    /\ cli' = "exited"
    /\ UNCHANGED <<interrupts, worker_state, worker_term, grace_expired, next_cli>>

\* Follow-on CLI acquires the work directory lock only when no lock holder remains.
NextCliAcquire ==
    /\ next_cli = "waiting"
    /\ LockHolders = {}
    /\ next_cli' = "acquired"
    /\ UNCHANGED <<cli, interrupts, worker_state, worker_term, grace_expired>>

\* Normal CLI completion when all workers finished without an interrupt.
CliNormalExit ==
    /\ cli = "scheduling"
    /\ \A w \in Workers : worker_state[w] = "reaped"
    /\ cli' = "exited"
    /\ UNCHANGED <<interrupts, worker_state, worker_term, grace_expired, next_cli>>

\* Terminal stuttering once follow-on CLI acquired the lock.
Done ==
    /\ next_cli = "acquired"
    /\ UNCHANGED vars

Next ==
    \/ \E w \in Workers : StartWorker(w)
    \/ \E w \in Workers : WorkerNaturalExit(w)
    \/ FirstSigint
    \/ SecondSigint
    \/ \E w \in Workers : SendSigterm(w)
    \/ \E w \in Workers : WorkerReapedAfterSigterm(w)
    \/ \E w \in Workers : GraceExpires(w)
    \/ \E w \in Workers : SendSigkillAndReap(w)
    \/ CliExit
    \/ CliNormalExit
    \/ NextCliAcquire
    \/ Done

Fairness ==
    /\ \A w \in Workers : WF_vars(SendSigterm(w))
    /\ \A w \in Workers : WF_vars(WorkerReapedAfterSigterm(w))
    /\ \A w \in Workers : WF_vars(GraceExpires(w))
    /\ \A w \in Workers : WF_vars(SendSigkillAndReap(w))
    /\ WF_vars(CliExit)
    /\ WF_vars(NextCliAcquire)

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK ==
    /\ cli \in {"scheduling", "draining", "exited"}
    /\ interrupts \in 0..2
    /\ worker_state \in [Workers -> {"idle", "running", "stopping", "reaped"}]
    /\ worker_term \in [Workers -> {"obeys", "ignores"}]
    /\ grace_expired \in [Workers -> BOOLEAN]
    /\ next_cli \in {"waiting", "acquired"}

\* Safety: CLI never exits while any worker is still running or stopping.
NoOrphanedWorkers ==
    cli = "exited" => ActiveWorkers = {}

\* Safety: Next CLI cannot acquire the work lock while CLI or any worker holds it.
MutualExclusion ==
    next_cli = "acquired" => LockHolders = {}

\* Safety: No worker can remain active while follow-on CLI owns the work directory.
NoLiveWorkerDuringFollowOn ==
    next_cli = "acquired" => ActiveWorkers = {}

\* Liveness: Once interrupted, CLI eventually reaps all workers, exits, and follow-on acquires lock.
EventuallyReapedAndAcquirable ==
    (interrupts > 0) ~> (cli = "exited" /\ ActiveWorkers = {} /\ next_cli = "acquired")

=============================================================================
