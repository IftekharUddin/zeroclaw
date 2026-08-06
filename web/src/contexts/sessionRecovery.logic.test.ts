import assert from 'node:assert/strict';
import test from 'node:test';

import {
  isTerminalRecoveryStatus,
  recoveryFailureOutcome,
  shouldBlockSending,
  shouldOfferRecoveryAction,
  recoveryMessageKey,
} from './sessionRecovery.logic.ts';

test('auth and missing-session responses are terminal', () => {
  assert.equal(isTerminalRecoveryStatus(401), true);
  assert.equal(isTerminalRecoveryStatus(403), true);
  assert.equal(isTerminalRecoveryStatus(404), true);
});

test('statuses that invite a retry keep polling', () => {
  // 408 and 429 are explicit "try again" signals; treating them as terminal
  // would strand a user over a transient blip.
  assert.equal(isTerminalRecoveryStatus(408), false);
  assert.equal(isTerminalRecoveryStatus(429), false);
});

test('server-side and transport failures keep polling', () => {
  assert.equal(isTerminalRecoveryStatus(500), false);
  assert.equal(isTerminalRecoveryStatus(503), false);
  // A null status is a transport error with no HTTP response at all.
  assert.equal(isTerminalRecoveryStatus(null), false);
});

test('recovery does not give up while retries remain', () => {
  assert.equal(
    recoveryFailureOutcome({ status: 500, failures: 1, maxFailures: 6 }),
    null,
  );
});

test('recovery gives up immediately on a terminal status', () => {
  assert.deepEqual(
    recoveryFailureOutcome({ status: 401, failures: 1, maxFailures: 6 }),
    { kind: 'unrecoverable', reason: 'rejected', retryable: true },
  );
});

test('recovery gives up once the retry budget is exhausted', () => {
  assert.deepEqual(
    recoveryFailureOutcome({ status: 500, failures: 6, maxFailures: 6 }),
    { kind: 'unrecoverable', reason: 'exhausted', retryable: true },
  );
});

// The bug this suite exists for: recovery failure left the composer locked
// with no way out. Stop only aborts a turn on the replacement socket, which
// was never attached to the detached turn, so no frame ever arrives to
// release the lock.
const terminalOutcomes = [
  recoveryFailureOutcome({ status: 401, failures: 1, maxFailures: 6 }),
  recoveryFailureOutcome({ status: 500, failures: 6, maxFailures: 6 }),
];

test('every terminal outcome blocks sending', () => {
  for (const outcome of terminalOutcomes) {
    assert.notEqual(outcome, null);
    assert.equal(shouldBlockSending(outcome!), true);
  }
});

test('sending is never blocked without offering a way out', () => {
  for (const outcome of terminalOutcomes) {
    assert.notEqual(outcome, null);
    assert.equal(
      shouldOfferRecoveryAction(outcome!),
      true,
      'a blocked composer with no recovery affordance is the permanent lockout',
    );
  }
});

test('the composer is released once state resolves', () => {
  const resolved = { kind: 'resolved' } as const;
  assert.equal(shouldBlockSending(resolved), false);
  assert.equal(shouldOfferRecoveryAction(resolved), false);
});

test('a rejection is distinguished from an exhausted retry budget', () => {
  // The two failures need different guidance: one points at auth/session,
  // the other at gateway availability.
  assert.equal(recoveryMessageKey('rejected'), 'agent.session_recovery_rejected');
  assert.equal(recoveryMessageKey('exhausted'), 'agent.session_recovery_error');
});
