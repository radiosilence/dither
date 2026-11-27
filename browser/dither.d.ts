// What app.ts hands to Datastar expressions in the templates
// (`data-on:click="dither.show('viewer'); …"`).
interface Window {
  dither: {
    show(id: string): void;
    moved(): void;
    remember(label: Element): void;
  };
}
