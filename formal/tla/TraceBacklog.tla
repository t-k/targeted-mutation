---------------------------- MODULE TraceBacklog ----------------------------
(* The runtime does not write a comparison's summary each time a distance that
   is already known comes closer to zero (a loop counting towards its bound does
   that in every round). A thread keeps the latest such summary in its backlog;
   the backlog is written when the thread ends and by a flusher at any time.
   Anything else that is new is written at once.

   One site, distances on one side. `Bug` switches on a wrong variant, to see
   that the properties tell it apart:
     "none"            the protocol as implemented (src/runtime/tmt_rt.rs)
     "no_flush_at_end" a thread ends without writing its backlog
     "keep_first"      the backlog keeps the first summary instead of the latest
     "forget_only"     news of the other kind clears the backlog and is not written *)
EXTENDS Naturals, FiniteSets

CONSTANTS Threads, Far, Bug

None == Far + 1
Dist == 1..Far

VARIABLES
    seen,     \* the nearest distance each thread has observed (None: nothing yet)
    kept,     \* what its backlog holds (None: nothing to write)
    pend,     \* a summary on its way to the trace, the tracer's lock not yet taken
    alive,
    written,  \* the distances in the trace file
    observed  \* ghost: every distance any thread has observed

vars == <<seen, kept, pend, alive, written, observed>>

Init ==
    /\ seen = [t \in Threads |-> None]
    /\ kept = [t \in Threads |-> None]
    /\ pend = [t \in Threads |-> None]
    /\ alive = [t \in Threads |-> TRUE]
    /\ written = {}
    /\ observed = {}

Ready(t) == alive[t] /\ pend[t] = None

\* The first observation, or one that is new in another way (a relation not seen
\* before) and may come with a nearer distance: written at once.
Other(t) ==
    /\ Ready(t)
    /\ \E d \in Dist :
        /\ d <= seen[t]
        /\ seen' = [seen EXCEPT ![t] = d]
        /\ observed' = observed \cup {d}
        /\ kept' = [kept EXCEPT ![t] = None]
        /\ pend' = [pend EXCEPT ![t] = IF Bug = "forget_only" THEN None ELSE d]
    /\ UNCHANGED <<alive, written>>

Write(t) ==
    /\ alive[t] /\ pend[t] # None
    /\ written' = written \cup {pend[t]}
    /\ pend' = [pend EXCEPT ![t] = None]
    /\ UNCHANGED <<seen, kept, alive, observed>>

\* A known distance comes closer: kept for later.
Closer(t) ==
    /\ Ready(t)
    /\ seen[t] # None
    /\ \E d \in Dist :
        /\ d < seen[t]
        /\ seen' = [seen EXCEPT ![t] = d]
        /\ observed' = observed \cup {d}
        /\ kept' = [kept EXCEPT ![t] =
                      IF Bug = "keep_first" /\ kept[t] # None THEN kept[t] ELSE d]
    /\ UNCHANGED <<pend, alive, written>>

\* The flusher, or the end of another thread, writes one backlog.
Drain(t) ==
    /\ kept[t] # None
    /\ written' = written \cup {kept[t]}
    /\ kept' = [kept EXCEPT ![t] = None]
    /\ UNCHANGED <<seen, pend, alive, observed>>

End(t) ==
    /\ Ready(t)
    /\ alive' = [alive EXCEPT ![t] = FALSE]
    /\ IF Bug = "no_flush_at_end" \/ kept[t] = None
         THEN UNCHANGED <<written, kept>>
         ELSE /\ written' = written \cup {kept[t]}
              /\ kept' = [kept EXCEPT ![t] = None]
    /\ UNCHANGED <<seen, pend, observed>>

Next == \E t \in Threads : Other(t) \/ Write(t) \/ Closer(t) \/ Drain(t) \/ End(t)

Spec == Init /\ [][Next]_vars

Min(S) == CHOOSE m \in S : \A x \in S : m <= x

TypeOK ==
    /\ seen \in [Threads -> Dist \cup {None}]
    /\ kept \in [Threads -> Dist \cup {None}]
    /\ pend \in [Threads -> Dist \cup {None}]
    /\ written \subseteq Dist

\* The trace never says more than was observed.
NothingInvented == written \subseteq observed

\* What a thread knows is in the trace, in its backlog or on its way.
NothingLost ==
    \A t \in Threads :
        seen[t] # None => seen[t] \in written \cup {kept[t], pend[t]}

\* When every thread has ended, the trace has the nearest distance.
CompleteAtTheEnd ==
    (\A t \in Threads : ~alive[t]) /\ observed # {} =>
        written # {} /\ Min(written) = Min(observed)
=============================================================================
