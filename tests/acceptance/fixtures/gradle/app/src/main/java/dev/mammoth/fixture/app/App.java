package dev.mammoth.fixture.app;

import dev.mammoth.fixture.Greeting;

/** The application module; acceptance tests edit this file to cause a miss in one module. */
public final class App {
    private App() {}

    public static void main(String[] args) {
        System.out.println(Greeting.of("mammoth"));
    }
}
