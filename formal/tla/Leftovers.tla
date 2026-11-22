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
(* is open have a copy as well ("passing"). They are children of the tool, let *)
(* go by themselves, and must never be stopped.                                *)
EXTENDS Naturals, FiniteSets

CONSTANTS Procs,      \* processes the run may start, the run itself included
          Passing,    \* commands of the tool on their way from fork to exec
          MaxForks    \* how many processes may be started after the run began

VARIABLES phase,      \* "open", "running", "sweeping", "done"
          has,        \* processes of the run that have the file
          copies,     \* passing commands that have a copy
          listed,     \* the listing the sweep is working on
          stopped,    \* everything the sweep ever stopped
          forks       \* processes started so far

vars == <<phase, has, copies, listed, stopped, forks>>

Init == /\ phase = "open"
        /\ has = {}
        /\ copies = {}
        /\ listed = {}
        /\ stopped = {}
        /\ forks = 0

\* Another command of the tool forks while the handle is open.
Pass(c) == /\ phase = "open"
           /\ c \notin copies
           /\ copies' = copies \cup {c}
           /\ UNCHANGED <<phase, has, listed, stopped, forks>>

\* It reaches its exec, or ends: the copy is closed.
Let(c) == /\ c \in copies
          /\ copies' = copies \ {c}
          /\ UNCHANGED <<phase, has, listed, stopped, forks>>

\* The run starts with the file, and the tool closes its handle.
Start(p) == /\ phase = "open"
            /\ has' = {p}
            /\ phase' = "running"
            /\ UNCHANGED <<copies, listed, stopped, forks>>

\* A process with the file starts another, which has it too.
Fork(p, q) == /\ phase \in {"running", "sweeping"}
              /\ p \in has
              /\ q \notin has /\ q \notin stopped
              /\ forks < MaxForks
              /\ has' = has \cup {q}
              /\ forks' = forks + 1
              /\ UNCHANGED <<phase, copies, listed, stopped>>

\* A process ends by itself.
Exit(p) == /\ phase \in {"running", "sweeping"}
           /\ p \in has
           /\ has' = has \ {p}
           /\ UNCHANGED <<phase, copies, listed, stopped, forks>>

\* The run is over (collected by the tool): from here the tool sweeps.
RunOver == /\ phase = "running"
           /\ phase' = "sweeping"
           /\ UNCHANGED <<has, copies, listed, stopped, forks>>

\* The lock is free: nobody has the file.
Free == /\ phase = "sweeping"
        /\ has = {} /\ copies = {}
        /\ phase' = "done"
        /\ UNCHANGED <<has, copies, listed, stopped, forks>>

\* The lock is held: list who has the file, without the children of the tool.
List == /\ phase = "sweeping"
        /\ has \cup copies # {}
        /\ listed = {}
        /\ listed' = has
        /\ UNCHANGED <<phase, has, copies, stopped, forks>>

\* Stop one of the listed. It may have ended since.
Stop(p) == /\ phase = "sweeping"
           /\ p \in listed
           /\ listed' = listed \ {p}
           /\ has' = has \ {p}
           /\ stopped' = stopped \cup {p}
           /\ UNCHANGED <<phase, copies, forks>>

Next == \/ \E c \in Passing : Pass(c) \/ Let(c)
        \/ \E p \in Procs : Start(p) \/ Exit(p) \/ Stop(p)
        \/ \E p, q \in Procs : Fork(p, q)
        \/ RunOver \/ Free \/ List
        \/ (phase = "done" /\ UNCHANGED vars)

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Free) /\ WF_vars(List)
        /\ \A p \in Procs : WF_vars(Stop(p))
        /\ \A c \in Passing : WF_vars(Let(c))

TypeOK == /\ phase \in {"open", "running", "sweeping", "done"}
          /\ has \subseteq Procs /\ copies \subseteq Passing
          /\ listed \subseteq Procs /\ stopped \subseteq Procs

\* A command of the tool is never stopped, and nothing is stopped before the run is over.
OnlyTheRuns == /\ stopped \cap Passing = {}
               /\ (phase \in {"open", "running"} => stopped = {})

\* When the sweep says it is done, nobody has the file.
DoneIsClean == phase = "done" => (has = {} /\ copies = {})

\* A run that is over is cleaned up in the end (the number of forks is bounded).
Cleans == (phase = "sweeping") ~> (phase = "done")
=============================================================================
