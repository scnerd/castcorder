// Shared run state: e2e.mjs fills this in; the check modules read it when called.
// The extra hosts (security, identity, cap checks) and their logs are set by
// the modules that start them.
export const ctx = { hostLog: '', host2: null, host2Log: '', host3: null, host3Log: '', host4: null, host4Log: '' };
