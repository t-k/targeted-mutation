-------------------------- MODULE CampaignCheckpoint --------------------------
EXTENDS FiniteSets, Sequences

CONSTANTS Phases, Jobs, Contexts, Absent

(* A context represents the campaign, phase, and ordered candidate identities.
   The persistent history holds only the most recent inventory per phase.
   The tail models one serialized write that can tear before it commits. *)
VARIABLES identities, committed, tail, running, active, done,
          before, reused, exact, complete

vars == <<identities, committed, tail, running, active, done,
          before, reused, exact, complete>>

Empty == [p \in Phases |-> {}]
NoTail == <<>>

Init ==
    /\ identities = [p \in Phases |-> Absent]
    /\ committed = Empty
    /\ tail = NoTail
    /\ running = FALSE
    /\ active = {}
    /\ done = {}
    /\ before = Empty
    /\ reused = Empty
    /\ exact = {}
    /\ complete = FALSE

Start ==
    /\ ~running
    /\ tail = NoTail
    /\ running' = TRUE
    /\ active' = {}
    /\ done' = {}
    /\ before' = Empty
    /\ reused' = Empty
    /\ exact' = {}
    /\ complete' = FALSE
    /\ UNCHANGED <<identities, committed, tail>>

Begin(p, ctx) ==
    /\ running
    /\ tail = NoTail
    /\ p \notin active
    /\ active' = active \cup {p}
    /\ before' = [before EXCEPT ![p] = committed[p]]
    /\ IF identities[p] = ctx
          THEN /\ exact' = exact \cup {p}
               /\ reused' = [reused EXCEPT ![p] = committed[p]]
               /\ UNCHANGED committed
          ELSE /\ committed' = [committed EXCEPT ![p] = {}]
               /\ UNCHANGED <<exact, reused>>
    /\ identities' = [identities EXCEPT ![p] = ctx]
    /\ UNCHANGED <<tail, running, done, complete>>

Write(p, j) ==
    /\ running
    /\ tail = NoTail
    /\ p \in active \ done
    /\ j \notin committed[p]
    /\ tail' = <<p, j>>
    /\ UNCHANGED <<identities, committed, running, active, done,
                  before, reused, exact, complete>>

Commit ==
    /\ running
    /\ tail # NoTail
    /\ LET p == tail[1]
           j == tail[2]
       IN committed' = [committed EXCEPT ![p] = @ \cup {j}]
    /\ tail' = NoTail
    /\ UNCHANGED <<identities, running, active, done,
                  before, reused, exact, complete>>

Crash ==
    /\ running
    /\ running' = FALSE
    /\ active' = {}
    /\ done' = {}
    /\ before' = Empty
    /\ reused' = Empty
    /\ exact' = {}
    /\ UNCHANGED <<identities, committed, tail, complete>>

Recover ==
    /\ ~running
    /\ tail # NoTail
    /\ tail' = NoTail
    /\ UNCHANGED <<identities, committed, running, active, done,
                  before, reused, exact, complete>>

FinishPhase(p) ==
    /\ running
    /\ tail = NoTail
    /\ p \in active \ done
    /\ committed[p] = Jobs
    /\ done' = done \cup {p}
    /\ UNCHANGED <<identities, committed, tail, running, active,
                  before, reused, exact, complete>>

FinishCampaign ==
    /\ running
    /\ tail = NoTail
    /\ active = done
    /\ running' = FALSE
    /\ complete' = TRUE
    /\ UNCHANGED <<identities, committed, tail, active, done, before, reused, exact>>

Next ==
    \/ Start
    \/ \E p \in Phases, ctx \in Contexts : Begin(p, ctx)
    \/ \E p \in Phases, j \in Jobs : Write(p, j)
    \/ Commit
    \/ Crash
    \/ Recover
    \/ \E p \in Phases : FinishPhase(p)
    \/ FinishCampaign

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ identities \in [Phases -> Contexts \cup {Absent}]
    /\ committed \in [Phases -> SUBSET Jobs]
    /\ tail \in {NoTail} \cup (Phases \X Jobs)
    /\ running \in BOOLEAN
    /\ active \subseteq Phases
    /\ done \subseteq active
    /\ before \in [Phases -> SUBSET Jobs]
    /\ reused \in [Phases -> SUBSET Jobs]
    /\ exact \subseteq active
    /\ complete \in BOOLEAN

ResumeOnlyExact ==
    \A p \in active :
        /\ reused[p] = IF p \in exact THEN before[p] ELSE {}
        /\ reused[p] \subseteq committed[p]

TailIsPending ==
    tail = NoTail \/ tail[2] \notin committed[tail[1]]

CompletionSound ==
    /\ \A p \in done : committed[p] = Jobs
    /\ complete => ~running /\ active = done /\ tail = NoTail

=============================================================================
