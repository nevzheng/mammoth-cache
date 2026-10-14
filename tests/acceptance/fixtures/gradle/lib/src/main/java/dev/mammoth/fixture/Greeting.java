package dev.mammoth.fixture;

/** A tiny library class so the fixture has a dependency between modules. */
public final class Greeting {
    private Greeting() {}

    public static String of(String name) {
        return "Hello, " + name + " (" + Salt.VALUE + ")";
    }
}
