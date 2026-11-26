// The selected memory, shared between views (Explore ↔ Memory map).
class Selection {
  id = $state<string | null>(null);
  select(id: string | null) {
    this.id = id;
  }
}
export const selection = new Selection();
