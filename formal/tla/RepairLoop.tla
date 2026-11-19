---------------------------- MODULE RepairLoop ----------------------------
(* The instrument -> cargo check -> disable loop in src/driver.rs.

   Some instrumentation sites do not compile (Bad). Each failed build blames a
   non-empty set of still-enabled sites; rustc may blame an innocent enclosing
   site, and it may report only some of the broken ones. An error that cannot be
   attributed to any enabled site aborts the run.

   Checked: the loop terminates, a successful build never contains a Bad site,
   and the number of rebuilds is bounded by the number of sites. *)
EXTENDS FiniteSets, Naturals

CONSTANTS Sites, Bad
ASSUME Bad \subseteq Sites

VARIABLES enabled, state, rounds
vars == <<enabled, state, rounds>>

Init == enabled = Sites /\ state = "building" /\ rounds = 0

BuildOk ==
    /\ state = "building"
    /\ enabled \cap Bad = {}
    /\ state' = "ok"
    /\ UNCHANGED <<enabled, rounds>>

Disable ==
    /\ state = "building"
    /\ enabled \cap Bad # {}
    /\ \E blamed \in (SUBSET enabled) \ {{}} :
          enabled' = enabled \ blamed
    /\ rounds' = rounds + 1
    /\ UNCHANGED state

Abort ==
    /\ state = "building"
    /\ enabled \cap Bad # {}
    /\ state' = "failed"
    /\ UNCHANGED <<enabled, rounds>>

Next == BuildOk \/ Disable \/ Abort

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

TypeOk ==
    /\ enabled \subseteq Sites
    /\ state \in {"building", "ok", "failed"}
    /\ rounds \in 0..Cardinality(Sites)

OkIsClean == state = "ok" => enabled \cap Bad = {}
Bounded == rounds + Cardinality(enabled) <= Cardinality(Sites)
Terminates == <>(state \in {"ok", "failed"})
=============================================================================
