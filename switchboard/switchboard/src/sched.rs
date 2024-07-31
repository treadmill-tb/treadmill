// questions:
//  (1) where should timeout be specified?
//      should it be per-job or a system setting? I feel like there's enough variation that we would
//      want to at least set it on a per-job basis.
//  (2) how should hard reservations be handled?
//  (3) what kind of UX flows do we want to support?
// I think the latter is really the most pressing question, because that will inform the
// functionality that we will have.
//
// Honestly, I think right now, everyone has very different visions for how this thing ought to
// work. This indicates to me that I should make the simplest possible mechanism.
//
// For now: we will add a queuing mechanism, but not perform any matching, and instead run it on a
// per-supervisor basis.
// Actually, if we want to have 1-job-1-run policy, it implies that a job only runs on one
// supervisor. This means that a "job" as such cannot specify multiple machines to run on. Therefore
// we need a concept of "job template" that can run on multiple machines.
// Or perhaps, we should rename Job -> Run, if these are the semantics that we are giving it.
// In any case, temporarily we have
//
//      Batch { Job Template + Supervisor Specifier } --> Jobs
//
// which should work sufficiently.
// We can then specify enqueue_batch or something similar; however, I don't think "batch" is a very
// good term since, at least in my mind, it's ambiguous towards heterogeneity.
