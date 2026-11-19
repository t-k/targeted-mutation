------------------------------ MODULE JobPool ------------------------------
(* The parallel mutant runner in src/driver.rs.

   Workers share one counter. A worker claims the next job by reading and
   advancing the counter in one atomic step (fetch_add), runs the job, and
   stores the result in the slot of that job. A worker stops when the counter
   is past the last job.

   Checked: no job runs twice, a result is only ever written by the worker
   that claimed the job, and when every worker has stopped every job has a
   result. *)
EXTENDS Naturals, FiniteSets

CONSTANTS Workers, NumJobs

VARIABLES next, claimed, result, pc
vars == <<next, claimed, result, pc>>

Jobs == 1..NumJobs

Init ==
    /\ next = 1
    /\ claimed = [w \in Workers |-> 0]
    /\ result = [j \in Jobs |-> 0]        \* 0: no result yet, otherwise the number of runs
    /\ pc = [w \in Workers |-> "idle"]

Claim(w) ==
    /\ pc[w] = "idle"
    /\ next <= NumJobs
    /\ claimed' = [claimed EXCEPT ![w] = next]
    /\ next' = next + 1
    /\ pc' = [pc EXCEPT ![w] = "running"]
    /\ UNCHANGED result

Stop(w) ==
    /\ pc[w] = "idle"
    /\ next > NumJobs
    /\ pc' = [pc EXCEPT ![w] = "done"]
    /\ UNCHANGED <<next, claimed, result>>

Finish(w) ==
    /\ pc[w] = "running"
    /\ result' = [result EXCEPT ![claimed[w]] = @ + 1]
    /\ pc' = [pc EXCEPT ![w] = "idle"]
    /\ UNCHANGED <<next, claimed>>

Next == \E w \in Workers : Claim(w) \/ Stop(w) \/ Finish(w)

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

NoJobRunsTwice == \A j \in Jobs : result[j] <= 1
DistinctClaims ==
    \A v, w \in Workers :
        (v # w /\ pc[v] = "running" /\ pc[w] = "running") => claimed[v] # claimed[w]
AllDoneMeansComplete ==
    (\A w \in Workers : pc[w] = "done") => \A j \in Jobs : result[j] = 1
Terminates == <>(\A w \in Workers : pc[w] = "done")
=============================================================================
