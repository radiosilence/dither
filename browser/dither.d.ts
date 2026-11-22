// What app.ts hands to Datastar expressions in the templates
// (`data-on:click="dither.show('viewer'); …"`).
interface Window {
  dither: {
    show(id: string): void;
    selecting(on: boolean): void;
    add(files: FileList | File[]): Promise<void>;
  };
}
