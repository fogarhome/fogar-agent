import FogarWire
import SwiftUI

/// The conversations, newest first, as the agent pushes them: «Nueva» on top, the open one on piedra
/// alta (like the sidebar's sections), and «en marcha» on any where Fogar is answering now, on this
/// Mac or another device (it also moves to the top). Nothing is kept by the app.
struct ListaConversaciones: View {
    @Environment(Conversacion.self) private var conv

    var body: some View {
        let abierta = conv.vista?.id
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Conversaciones").font(Tipo.etiqueta).suave()
                Spacer(minLength: 8)
                Button { conv.nuevaAhora() } label: {
                    Label("Nueva", systemImage: "square.and.pencil")
                }
                .buttonStyle(Pildora(pequena: true))
                // Already new: nothing open and nothing shown (a failed create still can be cleared).
                .disabled(!conv.vivo || abierta == nil && (conv.vista?.entries.isEmpty ?? true))
                .help("Empezar una conversación nueva")
            }
            .padding(.horizontal, 6)
            ScrollView {
                LazyVStack(spacing: 2) {
                    if conv.lista.isEmpty {
                        Text(conv.vivo ? "Aún no hay conversaciones. Escribe y empieza una." : "Sin conexión con el agente.")
                            .meta()
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 10)
                            .padding(.top, 6)
                    }
                    ForEach(conv.lista, id: \.id) { c in
                        FilaConversacion(conversacion: c, abierta: c.id == abierta) { conv.abrirAhora(c.id) }
                            .disabled(!conv.vivo)
                    }
                }
                .animation(.easeOut(duration: Medida.base), value: conv.lista.map(\.id))
            }
            .scrollIndicators(.automatic)
        }
        .padding(.horizontal, 12)
        .padding(.top, 20)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Conversaciones")
    }
}

private struct FilaConversacion: View {
    let conversacion: WireSummary
    let abierta: Bool
    let elegir: () -> Void
    @State private var hover = false

    var body: some View {
        let titulo = Textos.tituloConversacion(conversacion)
        Button(action: elegir) {
            VStack(alignment: .leading, spacing: 4) {
                Text(titulo)
                    .font(Tipo.texto(14, peso: 550))
                    .lineLimit(1)
                    .foregroundStyle(abierta || hover ? Color.hueso : Color.huesoSuave)
                if conversacion.running {
                    HStack(spacing: 6) {
                        Brasa(tam: 6)
                        Text("en marcha").meta()
                    }
                } else if !conversacion.private {
                    Text("De toda la casa").meta()
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .background {
                RoundedRectangle(cornerRadius: 10)
                    .fill(abierta ? Color.piedraAlta : hover ? Color.velo.opacity(0.5) : .clear)
            }
            .contentShape(RoundedRectangle(cornerRadius: 10))
        }
        .buttonStyle(.plain)
        .onHover { hover = $0 }
        .animation(.easeOut(duration: Medida.rapido), value: hover)
        .accessibilityLabel(titulo + (conversacion.running ? ", en marcha" : ""))
        .accessibilityAddTraits(abierta ? .isSelected : [])
    }
}
