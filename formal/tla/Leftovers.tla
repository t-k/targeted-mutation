------------------------------ MODULE Leftovers ------------------------------
(* What a run of the tests leaves running is found by a file it passes on.     *)
(* src/leftovers.rs.                                                           *)
(*                                                                             *)
(* The tool opens a file, locks it through that open description, starts the   *)
(* run with it and closes its own handle. Whatever the run starts has the file *)
(* too. After the run, the lock is held as long as some process has the file;  *)
(* the tool lists who has it and stops them, again and again, because a        *)
(* process may start another between the listing and the stop.                 *)
(*                                                                             *)
(* Other commands of the tool that are between fork and exec while its handle  *)
(* is open have a copy as well ("passing"). They are active children of the    *)
(* tool (`parent = "me"` and registered in `active`), let go by themselves,    *)
(* and must never be stopped. Under PID 1 or a subreaper (`subreaper = TRUE`), *)
(* orphaned descendants of the run also report `parent = "me"` once their      *)
(* parent exits, so `List` separates them from active sibling children using   *)
(* the exact `is_left_candidate` predicate from `src/leftovers.rs`.            *)
EXTENDS Naturals, FiniteSets

CONSTANTS Procs,      \* processes the run may start, the run itself included
          Passing,    \* active sibling children of the tool (e.g. fork..exec)
          MaxForks    \* how many processes may be started after the run began

AllProcs == Procs \cup Passing
Parents == Procs \cup {"me", "other", "none"}

VARIABLES subreaper,  \* whether this tool process is PID 1 or a subreaper
          phase,      \* "open", "running", "sweeping", "done"
          root,       \* initial direct child process started for the run (or "none")
          has,        \* processes of the run that currently hold the file
          copies,     \* active sibling children that temporarily hold a copy
          parent,     \* current parent of every process in AllProcs
          active,     \* ACTIVE_CHILDREN: live direct children owned by this tool
          listed,     \* the listing the sweep is working on
          stopped,    \* everything the sweep ever stopped
          forks       \* processes started so far

vars == <<subreaper, phase, root, has, copies, parent, active, listed, stopped, forks>>

Init == /\ subreaper \in {FALSE, TRUE}
        /\ phase = "open"
        /\ root = "none"
        /\ has = {}
        /\ copies = {}
        /\ parent = [p \in AllProcs |-> "none"]
        /\ active = {}
        /\ listed = {}
        /\ stopped = {}
        /\ forks = 0

\* Matches `is_left_candidate(pid, me, parent, active_child, subreaper)` in `src/leftovers.rs`.
IsLeftCandidate(p) ==
    /\ parent[p] # "none"
    /\ (parent[p] # "me" \/ (subreaper /\ p \notin active))

\* Another command of the tool forks while the handle is open; `STARTING` synchronizes
\* its registration in `active` (`ACTIVE_CHILDREN`) before any sweep inspects `active`.
Pass(c) == /\ phase = "open"
           /\ c \notin copies
           /\ copies' = copies \cup {c}
           /\ parent' = [parent EXCEPT ![c] = "me"]
           /\ active' = active \cup {c}
           /\ UNCHANGED <<subreaper, phase, root, has, listed, stopped, forks>>

\* The sibling command reaches its exec, or ends: the copy is closed and unregistered.
Let(c) == /\ c \in copies
          /\ copies' = copies \ {c}
          /\ parent' = [parent EXCEPT ![c] = "none"]
          /\ active' = active \ {c}
          /\ UNCHANGED <<subreaper, phase, root, has, listed, stopped, forks>>

\* The run starts with the file, registers its root child in `active`, and closes our handle.
Start(p) == /\ phase = "open"
            /\ root' = p
            /\ has' = {p}
            /\ parent' = [parent EXCEPT ![p] = "me"]
            /\ active' = active \cup {p}
            /\ phase' = "running"
            /\ UNCHANGED <<subreaper, copies, listed, stopped, forks>>

\* A process with the file starts another, which inherits the file with `p` as parent.
Fork(p, q) == /\ phase \in {"running", "sweeping"}
              /\ p \in has
              /\ q \notin has /\ q \notin stopped /\ q # root
              /\ forks < MaxForks
              /\ has' = has \cup {q}
              /\ parent' = [parent EXCEPT ![q] = p]
              /\ forks' = forks + 1
              /\ UNCHANGED <<subreaper, phase, root, copies, active, listed, stopped>>

\* Helper: when `p` exits, its living children in `has` are reparented to `"me"` under
\* PID 1 / subreaper, or to `"other"` (init) otherwise.
ReparentChildren(p) ==
    [x \in AllProcs |->
        IF x = p THEN "none"
        ELSE IF x \in has /\ parent[x] = p THEN (IF subreaper THEN "me" ELSE "other")
        ELSE parent[x]]

\* A process ends by itself (reparenting any living children it spawned).
Exit(p) == /\ phase \in {"running", "sweeping"}
           /\ p \in has
           /\ has' = has \ {p}
           /\ parent' = ReparentChildren(p)
           /\ active' = active \ {p}
           /\ UNCHANGED <<subreaper, phase, root, copies, listed, stopped, forks>>

\* The run's direct child has ended and been collected (`Watch::finish` clears its active entry).
RunOver == /\ phase = "running"
           /\ root \notin has
           /\ phase' = "sweeping"
           /\ UNCHANGED <<subreaper, root, has, copies, parent, active, listed, stopped, forks>>

\* The lock is free: nobody has the file.
Free == /\ phase = "sweeping"
        /\ has = {} /\ copies = {}
        /\ phase' = "done"
        /\ UNCHANGED <<subreaper, root, has, copies, parent, active, listed, stopped, forks>>

\* The lock is held: inspect EVERY holder (`has \cup copies`) and keep only those
\* matching `IsLeftCandidate`, without consulting whether `p \in Procs` or `p \in Passing`.
List == /\ phase = "sweeping"
        /\ has \cup copies # {}
        /\ listed = {}
        /\ listed' = {p \in has \cup copies : IsLeftCandidate(p)}
        /\ UNCHANGED <<subreaper, phase, root, has, copies, parent, active, stopped, forks>>

\* Stop one of the listed. It may have ended since; if it had living children, they reparent.
Stop(p) == /\ phase = "sweeping"
           /\ p \in listed
           /\ listed' = listed \ {p}
           /\ has' = has \ {p}
           /\ copies' = copies \ {p}
           /\ parent' = ReparentChildren(p)
           /\ active' = active \ {p}
           /\ stopped' = stopped \cup {p}
           /\ UNCHANGED <<subreaper, phase, root, forks>>

Next == \/ \E c \in Passing : Pass(c) \/ Let(c)
        \/ \E p \in Procs : Start(p) \/ Exit(p)
        \/ \E p \in AllProcs : Stop(p)
        \/ \E p, q \in Procs : Fork(p, q)
        \/ RunOver \/ Free \/ List
        \/ (phase = "done" /\ UNCHANGED vars)

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Free) /\ WF_vars(List)
        /\ \A p \in AllProcs : WF_vars(Stop(p))
        /\ \A c \in Passing : WF_vars(Let(c))

TypeOK == /\ subreaper \in {FALSE, TRUE}
          /\ phase \in {"open", "running", "sweeping", "done"}
          /\ root \in Procs \cup {"none"}
          /\ has \subseteq Procs
          /\ copies \subseteq Passing
          /\ parent \in [AllProcs -> Parents]
          /\ active \subseteq AllProcs
          /\ listed \subseteq AllProcs
          /\ stopped \subseteq AllProcs

\* An active sibling command of the tool is never stopped, and nothing is stopped before sweep.
OnlyTheRuns == /\ stopped \cap Passing = {}
               /\ (phase \in {"open", "running"} => stopped = {})

\* When the sweep says it is done, nobody has the file.
DoneIsClean == phase = "done" => (has = {} /\ copies = {})

\* A run that is over is cleaned up in the end (even when orphans reparent to `"me"`).
Cleans == (phase = "sweeping") ~> (phase = "done")
=============================================================================
