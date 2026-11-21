------------------------------ MODULE TestPoll ------------------------------
EXTENDS Naturals

\* Times are abstract ticks. In the runner the first interval is 250 us.
\* Wakeup can be later than the requested sleep because of scheduling.
CONSTANT Deadline, Cap, Jitter
VARIABLES now, delay, outcome, requested, lastPoll, exit, failureLine
vars == <<now, delay, outcome, requested, lastPoll, exit, failureLine>>
Exit == {"none", "pass", "fail"}
Min(a, b) == IF a < b THEN a ELSE b

Init ==
    /\ now = 0
    /\ delay = 1
    /\ outcome = "running"
    /\ requested = 0
    /\ lastPoll = 0
    /\ exit = "none"
    /\ failureLine = FALSE

Poll ==
    /\ outcome = "running"
    /\ exit' \in Exit
    /\ failureLine' \in BOOLEAN
    /\ lastPoll' = now
    /\ outcome' = IF exit' # "none" THEN exit'
                   ELSE IF failureLine' THEN "fail"
                   ELSE IF now >= Deadline THEN "timeout"
                   ELSE "running"
    /\ IF outcome' = "running"
       THEN /\ requested' = Min(delay, Deadline - now)
            /\ delay' = Min(delay * 2, Cap)
            /\ now' \in (now + requested')..(now + requested' + Jitter)
       ELSE /\ requested' = 0
            /\ UNCHANGED <<now, delay>>

Done == outcome # "running" /\ UNCHANGED vars
Next == Poll \/ Done
Spec == Init /\ [][Next]_vars

BoundedDelay == delay \in 1..Cap
SleepWithinDeadline == requested > 0 => requested <= Cap /\ lastPoll + requested <= Deadline
HonourExit == exit # "none" => outcome = exit
HonourFailure == exit = "none" /\ failureLine => outcome = "fail"
TimeoutAfterChecks == outcome = "timeout" => now >= Deadline /\ exit = "none" /\ ~failureLine
=============================================================================
