import FogarWire
import ServiceManagement
import SwiftUI

/// Ajustes: only what is real today. Unpair (the agent forgets the key), appearance, open at login
/// (SMAppService, the system's own login item), and what this build is. Shown as a section of the
/// window and as the Settings scene (⌘,).
struct AjustesView: View {
    let enVentana: Bool
    @Environment(AgentLink.self) private var link

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 36) {
                if enVentana { Text("Ajustes").titularFogar() }
                Emparejamiento(enVentana: enVentana)
                AparienciaBloque()
                InicioSesion()
                AcercaDe(socket: link.socketPath)
            }
            .padding(.horizontal, enVentana ? 40 : 28)
            .padding(.top, enVentana ? 28 : 24)
            .padding(.bottom, 40)
            .frame(maxWidth: enVentana ? 720 : .infinity, alignment: .leading)
            // A form keeps its column; centred, so a wide window has no empty band on one side.
            .frame(maxWidth: .infinity)
        }
        // Settings window: the usual macOS width, never stretched across the screen.
        .frame(width: enVentana ? nil : 580, height: enVentana ? nil : 640)
        .background(Color.noche)
        .tint(.hueso)
        .navigationTitle("Ajustes")
    }
}

private struct Bloque<Content: View>: View {
    let titulo: String
    @ViewBuilder let content: Content
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(titulo).font(Tipo.h2).foregroundStyle(Color.hueso).accessibilityAddTraits(.isHeader)
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct Emparejamiento: View {
    let enVentana: Bool
    @Environment(AgentLink.self) private var link
    @Environment(Navegacion.self) private var nav
    @Environment(\.openWindow) private var openWindow
    @State private var confirmar = false
    @State private var trabajando = false
    @State private var error: String?

    var body: some View {
        Bloque(titulo: "Emparejamiento") {
            switch link.state {
            case .live(let s) where s.phase != .notPaired:
                Text(s.gateway.map { "Este Mac está emparejado con \($0.name) (\($0.address))." } ?? "Este Mac está emparejado.")
                    .font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                    .fixedSize(horizontal: false, vertical: true)
                Text("Al desemparejarlo, el agente borra su clave y deja de hablar con el gateway. Para volver, hará falta un código nuevo.")
                    .meta().fixedSize(horizontal: false, vertical: true)
                Button("Desemparejar este Mac…") { confirmar = true }
                    .buttonStyle(Pildora(ocupado: trabajando))
                    .disabled(trabajando)
                    .confirmationDialog(
                        s.gateway.map { "¿Desemparejar este Mac de \($0.name)?" } ?? "¿Desemparejar este Mac?",
                        isPresented: $confirmar
                    ) {
                        Button("Desemparejar", role: .destructive, action: desemparejar)
                        Button("Cancelar", role: .cancel) {}
                    } message: {
                        Text("El agente borra la clave de este Mac y se desconecta. Para volver, necesitarás un código nuevo del gateway.")
                    }
            case .live:
                Text("Este Mac no está emparejado.").font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                Button("Emparejar este Mac…") {
                    nav.seccion = .emparejar
                    if !enVentana { openWindow(id: "main") }
                }
                .buttonStyle(Pildora())
            default:
                Text("Sin el agente no se sabe con qué gateway está emparejado este Mac.")
                    .font(Tipo.cuerpo).suave()
            }
            if let error {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    EstadoIcono(tono: .bad, tam: 12)
                    Text(error).font(Tipo.cuerpo).foregroundStyle(Color.hueso)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }

    private func desemparejar() {
        trabajando = true
        error = nil
        Task {
            defer { trabajando = false }
            do { try await link.unpair() } catch { self.error = Textos.fallo(error) }
        }
    }
}

private struct AparienciaBloque: View {
    @AppStorage("apariencia") private var apariencia = Apariencia.sistema
    var body: some View {
        Bloque(titulo: "Apariencia") {
            Picker(selection: $apariencia) {
                ForEach(Apariencia.allCases) { Text($0.titulo).tag($0) }
            } label: { EmptyView() }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityLabel("Apariencia")
            .fixedSize()
            Text("«Automática» sigue el modo claro u oscuro de tu Mac.")
                .meta().fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct InicioSesion: View {
    @State private var estado = SMAppService.mainApp.status
    @State private var error: String?

    var body: some View {
        Bloque(titulo: "Al iniciar sesión") {
            Toggle("Abrir Fogar Desktop al iniciar sesión", isOn: Binding(get: { estado == .enabled }, set: { cambiar($0) }))
                .toggleStyle(.switch)
                .accessibilityLabel("Abrir Fogar Desktop al iniciar sesión")
                .font(Tipo.cuerpo)
                .foregroundStyle(Color.hueso)
            Group {
                if let error {
                    Text(error)
                } else if estado == .requiresApproval {
                    HStack(spacing: 6) {
                        Text("Falta tu permiso en Ajustes del Sistema.")
                        Button("Abrir Ítems de inicio") { SMAppService.openSystemSettingsLoginItems() }
                            .buttonStyle(.link)
                    }
                } else {
                    Text("Solo abre esta app; el agente se arranca aparte.")
                }
            }
            .meta()
        }
        .onAppear { estado = SMAppService.mainApp.status }
    }

    private func cambiar(_ on: Bool) {
        error = nil
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
        } catch {
            self.error = "macOS no ha dejado cambiarlo: \(error.localizedDescription)"
        }
        estado = SMAppService.mainApp.status
    }
}

private struct AcercaDe: View {
    let socket: String
    var body: some View {
        Bloque(titulo: "Acerca de") {
            VStack(alignment: .leading, spacing: 4) {
                Text("Fogar Desktop \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "en desarrollo") · protocolo con el agente v\(ipcVersion)")
                Text("Habla con el agente por \(socket.replacingOccurrences(of: NSHomeDirectory(), with: "~"))")
                    .textSelection(.enabled)
            }
            .meta()
        }
    }
}
