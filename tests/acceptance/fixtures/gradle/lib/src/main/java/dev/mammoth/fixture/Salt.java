package dev.mammoth.fixture;

/** Replaced in every copy the harness makes, so each run has fresh cache keys. */
final class Salt {
    static final String VALUE = "fixture";

    private Salt() {}
}
