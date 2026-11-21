--------------------------- MODULE CampaignPhases ---------------------------
(* Ordered reporting is independent of the order in which workers finish.
   A parallel timeout is provisional until the requested serial retry ends. *)
EXTENDS Naturals, FiniteSets

CONSTANT Baselines, Mutants, WorkerLimit, BaselineLimit
VARIABLES base, jobs, phase, policy, recorded

finalStates == {"pass", "fail", "timeout", "unconfirmed", "unable"}
vars == <<base, jobs, phase, policy, recorded>>

Init ==
    /\ base = [b \in Baselines |-> "waiting"]
    /\ jobs = [m \in Mutants |-> "waiting"]
    /\ phase = "baseline"
    /\ policy \in {"serial", "none"}
    /\ recorded = {}

BaselineBegin ==
    /\ phase = "baseline"
    /\ \A b \in Baselines : base[b] # "fail"
    /\ Cardinality({b \in Baselines : base[b] = "active"}) < BaselineLimit
    /\ \E b \in Baselines :
        /\ base[b] = "waiting"
        /\ base' = [base EXCEPT ![b] = "active"]
    /\ UNCHANGED <<jobs, phase, policy, recorded>>

BaselineEnd ==
    /\ phase = "baseline"
    /\ \E b \in Baselines, verdict \in {"pass", "fail"} :
        /\ base[b] = "active"
        /\ base' = [base EXCEPT ![b] = verdict]
    /\ UNCHANGED <<jobs, phase, policy, recorded>>

StartMutants ==
    /\ phase = "baseline"
    /\ (\A b \in Baselines : base[b] = "pass")
        \/ ((\E b \in Baselines : base[b] = "fail")
            /\ (\A b \in Baselines : base[b] # "active"))
    /\ phase' = IF \A b \in Baselines : base[b] = "pass" THEN "first" ELSE "error"
    /\ UNCHANGED <<base, jobs, policy, recorded>>

FirstBegin ==
    /\ phase = "first"
    /\ Cardinality({m \in Mutants : jobs[m] = "active"}) < WorkerLimit
    /\ \E m \in Mutants :
        /\ jobs[m] = "waiting"
        /\ jobs' = [jobs EXCEPT ![m] = "active"]
    /\ UNCHANGED <<base, phase, policy, recorded>>

FirstEnd ==
    /\ phase = "first"
    /\ \E m \in Mutants, verdict \in {"pass", "fail", "timeout"} :
        /\ jobs[m] = "active"
        /\ LET next == IF verdict # "timeout" \/ WorkerLimit = 1 THEN verdict
                       ELSE IF policy = "none" THEN "unconfirmed" ELSE "pending"
           IN /\ jobs' = [jobs EXCEPT ![m] = next]
              /\ recorded' = IF next \in finalStates THEN recorded \cup {m} ELSE recorded
    /\ UNCHANGED <<base, phase, policy>>

StartRetries ==
    /\ phase = "first"
    /\ \A m \in Mutants : jobs[m] \notin {"waiting", "active"}
    /\ phase' = "recheck"
    /\ UNCHANGED <<base, jobs, policy, recorded>>

RetryBegin ==
    /\ phase = "recheck"
    /\ \A m \in Mutants : jobs[m] # "active"
    /\ \E m \in Mutants :
        /\ jobs[m] = "pending"
        /\ jobs' = [jobs EXCEPT ![m] = "active"]
    /\ UNCHANGED <<base, phase, policy, recorded>>

RetryEnd ==
    /\ phase = "recheck"
    /\ \E m \in Mutants, result \in finalStates \ {"unconfirmed"} :
        /\ jobs[m] = "active"
        /\ jobs' = [jobs EXCEPT ![m] = result]
        /\ recorded' = recorded \cup {m}
    /\ UNCHANGED <<base, phase, policy>>

Finish ==
    /\ phase = "recheck"
    /\ recorded = Mutants
    /\ phase' = "done"
    /\ UNCHANGED <<base, jobs, policy, recorded>>

Stop == phase \in {"done", "error"} /\ UNCHANGED vars

Next == BaselineBegin \/ BaselineEnd \/ StartMutants \/ FirstBegin \/ FirstEnd
        \/ StartRetries \/ RetryBegin \/ RetryEnd \/ Finish \/ Stop
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

NoPrematureResults == recorded = {m \in Mutants : jobs[m] \in finalStates}
BaselineRequired == phase \in {"first", "recheck", "done"} =>
                      \A b \in Baselines : base[b] = "pass"
BoundedWorkers ==
    /\ Cardinality({b \in Baselines : base[b] = "active"}) <= BaselineLimit
    /\ Cardinality({m \in Mutants : jobs[m] = "active"}) <=
           IF phase = "recheck" THEN 1 ELSE WorkerLimit
Policy == policy = "serial" => \A m \in Mutants : jobs[m] # "unconfirmed"
Terminates == <>(phase \in {"done", "error"})
=============================================================================
