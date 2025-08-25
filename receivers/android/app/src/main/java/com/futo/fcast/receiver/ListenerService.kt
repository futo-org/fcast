package com.futo.fcast.receiver

import android.util.Log
import com.futo.fcast.receiver.models.EventMessage
import com.futo.fcast.receiver.models.EventObject
import com.futo.fcast.receiver.models.EventSubscribeObject
import com.futo.fcast.receiver.models.EventType
import com.futo.fcast.receiver.models.KeyDownEvent
import com.futo.fcast.receiver.models.KeyEvent
import com.futo.fcast.receiver.models.KeyUpEvent
import com.futo.fcast.receiver.models.Opcode
import java.util.UUID

abstract class ListenerService {
    protected val sessionMap: MutableMap<UUID, FCastSession> = mutableMapOf()
    private val _eventSubscribers: MutableMap<UUID, MutableList<EventSubscribeObject>> = mutableMapOf()

    abstract fun start()
    abstract fun stop()
    abstract fun disconnect(sessionId: UUID)

    fun <T> send(opcode: Opcode, message: T? = null, sessionId: UUID? = null) {
//        Log.i(TAG, "Sending message $message")

        if (sessionId != null) {
            if (opcode == Opcode.Event && !this.isSubscribedToEvent(sessionId, (message as EventMessage).event)) {
                return
            }

            try {
                this.sessionMap[sessionId]?.send(opcode, message)
            } catch (e: Throwable) {
                Log.w(TAG, "Failed to send error.", e)
                this.sessionMap[sessionId]?.close()
            }
        }
        else {
            for (session in this.sessionMap.values) {
                if (opcode == Opcode.Event && !this.isSubscribedToEvent(session.id, (message as EventMessage).event)) {
                    continue
                }

                try {
                    session.send(opcode, message)
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to send error.", e)
                    session.close()
                }
            }
        }
    }

    fun subscribeEvent(sessionId: UUID, event: EventSubscribeObject) {
        val sessionSubscriptions = _eventSubscribers.getOrDefault(sessionId, mutableListOf<EventSubscribeObject>())
        sessionSubscriptions += event
        _eventSubscribers[sessionId] = sessionSubscriptions
    }

    fun unsubscribeEvent(sessionId: UUID, event: EventSubscribeObject) {
        if (_eventSubscribers.containsKey(sessionId)) {
            val sessionSubscriptions = _eventSubscribers[sessionId]
            sessionSubscriptions?.remove(event)
            _eventSubscribers[sessionId] = sessionSubscriptions!!
        }
    }

    fun getSessions(): List<UUID> {
        return sessionMap.keys.toList()
    }

    fun getSessionProtocolVersion(sessionId: UUID): Long? {
        return sessionMap[sessionId]?.protocolVersion
    }

    fun getAllSubscribedKeys(): Pair<Set<String>, Set<String>> {
        val keyDown = mutableSetOf<String>()
        val keyUp = mutableSetOf<String>()

        for (session in _eventSubscribers.values) {
            for (event in session) {
                when (event.type) {
                    EventType.KeyDown -> keyDown += (event as KeyDownEvent).keys
                    EventType.KeyUp -> keyUp += (event as KeyUpEvent).keys
                    else -> {}
                }
            }
        }

        return Pair(keyDown, keyUp)
    }

    private fun isSubscribedToEvent(sessionId: UUID, event: EventObject): Boolean {
        var isSubscribed = false

        if (_eventSubscribers.containsKey(sessionId)) {
            for (e in _eventSubscribers[sessionId] ?: arrayListOf()) {
                if (e.type == event.type) {
                    when (e.type) {
                        EventType.KeyDown -> {
                            val subscribeEvent = e as KeyDownEvent
                            val keyEvent = event as KeyEvent
                            if (!subscribeEvent.keys.contains(keyEvent.key)) {
                                continue
                            }
                        }
                        EventType.KeyUp -> {
                            val subscribeEvent = e as KeyUpEvent
                            val keyEvent = event as KeyEvent
                            if (!subscribeEvent.keys.contains(keyEvent.key)) {
                                continue
                            }
                        }
                        else -> {}
                    }

                    isSubscribed = true
                    break
                }
            }
        }

        return isSubscribed
    }
// TODO: Address error handling for UI 2.0
//    protected async handleServerError(err: NodeJS.ErrnoException) {
//        errorHandler(err)
//    }

    companion object {
        private const val TAG = "ListenerService"
    }
}