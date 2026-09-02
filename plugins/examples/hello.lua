local counter = 0

function on_init()
    moku.show_info("Hello Lua plugin loaded!")
end

function on_event(key)
    if key == "up" then
        counter = counter + 1
        return true
    elseif key == "down" then
        counter = counter - 1
        return true
    elseif key == "esc" or key == "q" then
        moku.navigate_to("launcher")
        return true
    end
    return false
end

function on_draw()
    return {
        title = "Hello Lua",
        lines = {
            "This screen is fully rendered by a Lua script.",
            "",
            "Counter: " .. counter,
            "",
            "[up/down] Change counter | [q/esc] Exit",
        }
    }
end
